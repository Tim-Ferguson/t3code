//! The timeline rows for the turn items that are not messages: how each one reads, from the item
//! and the projection alone. A card holds only its own open/closed state, and its body is not
//! built until it is opened, so a long output or a streaming thought costs nothing while closed.
//! Wording follows the original (`MessagesTimeline.tsx`, `session-logic.ts`, `proposedPlan.ts`).
use crate::{markdown::Markdown, runtime::{self, UiModel}};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::{timeline_items as words, work_log};

/// Whether work the projection calls in progress is still the turn's: an item that outlives its
/// run is background work (or a stale marker), never "the agent is working".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Live {
    /// Not in progress.
    No,
    /// In progress, in the run that is still going.
    Yes,
    /// In progress after its run ended.
    Background,
}

pub(crate) fn in_progress(item: &Value) -> bool {
    matches!(item["status"].as_str(), Some("pending" | "running" | "waiting"))
}
pub(crate) fn liveness(item: &Value, active_run: Option<&str>) -> Live {
    if !in_progress(item) {
        Live::No
    } else if active_run.is_some() && item["runId"].as_str() == active_run {
        Live::Yes
    } else if item["status"] == "running" {
        Live::Background
    } else {
        // Waiting or queued in a run that is over: what the projection says, not live work.
        Live::No
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// The row's one-line label (`workEntryDisplayLabel` and the item's own title).
pub(crate) fn label(item: &Value) -> String {
    let title = item["title"].as_str().map(str::trim).filter(|title| !title.is_empty());
    let first = |text: &str| text.trim().lines().next().unwrap_or_default().to_owned();
    match item["type"].as_str().unwrap_or_default() {
        "command_execution" => item["input"].as_str().filter(|input| !input.trim().is_empty()).map(first).or_else(|| title.map(str::to_owned)).unwrap_or_else(|| "Ran command".into()),
        "file_change" => {
            let paths: Vec<&str> = item["changes"].as_array().map(|changes| changes.iter().filter_map(|change| change["path"].as_str()).collect()).filter(|paths: &Vec<&str>| !paths.is_empty()).unwrap_or_else(|| item["fileName"].as_str().into_iter().collect());
            match paths.as_slice() {
                [] => title.unwrap_or("File change").to_owned(),
                [one] => (*one).to_owned(),
                [one, rest @ ..] => format!("{one} +{} more", rest.len()),
            }
        }
        "web_search" => {
            let patterns: Vec<&str> = item["patterns"].as_array().map(|list| list.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if patterns.is_empty() { capitalize(title.unwrap_or("Searched the web")) } else { patterns.join(", ") }
        }
        "file_search" => title.map(str::to_owned).or_else(|| item["pattern"].as_str().map(|pattern| format!("Searched for {pattern}"))).unwrap_or_else(|| "Searched files".into()),
        "dynamic_tool" => capitalize(title.or_else(|| item["toolName"].as_str()).unwrap_or("Tool call")),
        "approval_request" => title.unwrap_or("Approval requested").to_owned(),
        "user_input_request" => title.map(str::to_owned).unwrap_or_else(|| if item["questionAnswer"].is_object() { "Answered questions".into() } else { "Input requested".into() }),
        "handoff" => "Context handoff".into(),
        "fork" => "Conversation fork".into(),
        "thread_created" => format!("Created thread{}", title.map(|title| format!(" · {title}")).unwrap_or_default()),
        "secret_request" => item["label"].as_str().unwrap_or("Secret requested").to_owned(),
        "subagent" => title.unwrap_or("Subagent").to_owned(),
        other => capitalize(&title.map(str::to_owned).unwrap_or_else(|| other.replace('_', " "))),
    }
}

/// The small word beside a row: why it is not simply done. Completed work shows none.
pub(crate) fn status_word(item: &Value, live: Live) -> Option<&'static str> {
    // A tool the provider declined to run (denied, cancelled before it started) says so, not just "cancelled".
    if live == Live::No && item["status"] != "completed" {
        if let Some(word) = words::non_execution_word(item["toolNonExecutionKind"].as_str()) {
            return Some(word);
        }
    }
    match (item["status"].as_str()?, live) {
        (_, Live::Background) => Some("background"),
        ("running", _) => Some("running"),
        ("waiting", _) => Some("waiting"),
        ("pending", _) => Some("queued"),
        ("failed", _) => Some("failed"),
        ("cancelled", _) => Some("cancelled"),
        ("interrupted", _) => Some("interrupted"),
        _ => None,
    }
}

/// The meta line under a settled response's final answer (`AssistantMessageMeta`): the item's
/// status when it is not simply done, a copy button while there is text to copy, and when the
/// answer was last written. The time is the answer's last update; nothing here ticks.
#[component]
pub(crate) fn AssistantFooter(text: String, status: String, updated_at: String, streaming: bool, fork: Option<(String, String)>, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let mut copied = use_signal(|| false);
    let offset = crate::scheduled_tasks_page::zone(chrono::Utc::now().timestamp_millis());
    let time = words::day_aware_timestamp(&updated_at, offset, chrono::Utc::now());
    let tooltip = words::timestamp_tooltip(&updated_at, offset);
    let can_copy = !streaming && !text.trim().is_empty();
    rsx! {div {class:"assistant-footer","data-item-type":"assistant_footer",
        if status != "completed" && !status.is_empty() {span {class:"badge mono",title:"Item status","{status}"}}
        if can_copy {button {class:"icon-button",r#type:"button","aria-label":if copied() {"Copied"} else {"Copy message"},title:"Copy message",
            onclick:move |_| {
                let text = text.clone();
                spawn(async move {
                    if crate::settings_common::copy_text(&text).await {
                        copied.set(true);
                        runtime::delay(1500).await;
                        copied.set(false);
                    }
                });
            },
            if copied() {"✓"} else {"⧉"}}}
        if let Some((source_thread, run_id)) = fork.filter(|_| !streaming) {crate::thread_relations::ForkButton {state,transport:transport.clone(),source_thread,run_id}}
        if !streaming && !time.is_empty() {time {class:"muted",title:"{tooltip}",datetime:"{updated_at}","{time}"}}
    }}
}

/// A report that background work finished: the work's own glyph, what happened, and when. A report
/// about a subagent the parent knows is drawn as that subagent instead (`subagent_rows`).
#[component]
pub(crate) fn Notification(item: Value, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let view = words::notification_view(&item);
    let summary = item["summary"].as_str().unwrap_or_default().to_owned();
    let glyph = match view.icon {
        "bot" => "◈",
        "terminal" => "›_",
        "eye" => "◉",
        "circle-alert" => "!",
        _ => "ϟ",
    };
    let at = item["startedAt"].as_str().or_else(|| item["updatedAt"].as_str()).map(|at| words::short_timestamp(at, crate::scheduled_tasks_page::zone(chrono::Utc::now().timestamp_millis()))).unwrap_or_default();
    let open = view.child_thread_id.clone();
    rsx! {div {class:if view.failed {"activity-notice notification failure"} else {"activity-notice notification"},role:"note","data-item-type":"notification",
        span {class:"work-mark","aria-hidden":"true","{glyph}"}
        span {class:"notification-summary","{summary}"}
        if let Some(child) = open {button {class:"link-button","aria-label":"Open subagent thread",onclick:move |_|runtime::select_thread(&transport,state,child.clone()),"Open subagent"}}
        span {class:"notification-time muted","{at}"}
    }}
}

#[component]
pub(crate) fn Disclosure(class: String, kind: String, name: String, summary: Element, open: Signal<bool>, children: Element) -> Element {
    rsx! {
        details {class:"{class}","data-item-type":"{kind}",open:open(),
            summary {"aria-label":"{name}","aria-expanded":"{open()}",onclick:move |event| {event.prevent_default();let now=*open.peek();open.set(!now);},{summary}}
            if open() {div {class:"activity-body",{children}}}
        }
    }
}

#[component]
pub(crate) fn Reasoning(item: Value, live: Live) -> Element {
    let open = use_signal(|| false);
    let text = item["text"].as_str().unwrap_or_default().to_owned();
    let title = item["title"].as_str().map(str::trim).filter(|title| !title.is_empty()).map(str::to_owned);
    let verb = if live == Live::Yes { "Thinking" } else { "Thought" };
    let preview = title.clone().unwrap_or_else(|| words::live_thought_line(&text));
    let stopped = matches!(item["status"].as_str(), Some("interrupted" | "cancelled" | "failed")).then(|| item["status"].as_str().unwrap_or_default().to_owned());
    rsx! {Disclosure {class:"tool-activity reasoning",kind:"reasoning",name:format!("{verb} {preview}"),open,
        summary:rsx!{span {class:"activity-title","{verb}"}span {class:"activity-preview",title:"{preview}","{preview}"}if let Some(word)=stopped {span {class:"activity-status","{word}"}}},
        if let Some(title)=title.filter(|_|!text.is_empty()) {p {class:"reasoning-title",strong {"{title}"}}}
        if !text.is_empty() {Markdown {text}} else {p {class:"muted","No thought text."}}
    }}
}

#[component]
pub(crate) fn Compaction(item: Value) -> Element {
    let label = words::compaction_label(&item);
    let detail = item["summary"].as_str().map(str::to_owned);
    rsx! {div {class:"system-divider",role:"separator","aria-label":"{label}","data-item-type":"compaction",
        span {class:"divider-line"}span {class:"divider-label","⇣ {label}"}span {class:"divider-line"}
        if let Some(detail)=detail {p {class:"divider-detail muted","{detail}"}}
    }}
}

#[component]
pub(crate) fn RunNotice(item: Value) -> Element {
    let kind = item["type"].as_str().unwrap_or_default();
    let message = item["message"].as_str().unwrap_or_default().to_owned();
    let (label, class) = match kind {
        "run_interrupt_request" => ("Interrupt requested", "activity-notice interrupt"),
        "run_interrupt_result" => ("Run interrupted", "system-divider danger"),
        _ => ("", "activity-notice"),
    };
    rsx! {aside {class:"{class}",role:"note","data-item-type":"{kind}",
        if !label.is_empty() {strong {"{label}"}" · "}
        "{message}"
    }}
}

/// An error or a retry. Open to read the provider's message; the heading carries the retry state.
#[component]
pub(crate) fn ErrorRow(item: Value) -> Element {
    let offset_minutes = crate::scheduled_tasks_page::zone(chrono::Utc::now().timestamp_millis());
    let open = use_signal(|| false);
    let (heading, detail) = words::error_presentation(&item);
    let tone = match words::error_tone(&item) {
        words::Tone::Success => "success",
        words::Tone::Warning => "warning",
        words::Tone::Danger => "danger",
    };
    let usage_limit = item["failure"]["class"] == "usage_limit" && item["status"] == "failed";
    let reset = item["failure"]["resetAt"].as_str().map(|at| words::upcoming_timestamp(at, offset_minutes, chrono::Utc::now())).filter(|time| !time.is_empty());
    let headline = if usage_limit { format!("Usage limit reached.{}", reset.map(|time| format!(" Retry after {time}.")).unwrap_or_default()) } else { heading.clone() };
    // A failure says what went wrong where it happened; only a usage limit keeps its message away.
    if item["status"] == "failed" && !usage_limit {
        return rsx! {section {class:"tool-activity error-row {tone}",role:"alert","data-item-type":"error",
            div {class:"activity-body",strong {class:"error-title","{headline}"}p {class:"error-detail","{detail}"}}
        }};
    }
    let status = (item["status"] != "completed").then(|| item["status"].as_str().unwrap_or_default().to_owned());
    rsx! {Disclosure {class:"tool-activity error-row {tone}",kind:"error",name:headline.clone(),open,
        summary:rsx!{span {class:"activity-title error-title",role:"alert","{headline}"}if let Some(status)=status {span {class:"activity-status","{status}"}}},
        p {class:"error-detail","{detail}"}
    }}
}

/// The streaming Markdown plan, collapsed when long, with the original's actions.
#[component]
pub(crate) fn PlanCard(item: Value, state: Store<UiModel>, transport: runtime::TransportHandle, thread_id: String, on_copy: EventHandler<String>, on_download: EventHandler<(String, String)>) -> Element {
    let mut expanded = use_signal(|| false);
    let mut actions = use_signal(|| false);
    let mut saving = use_signal(|| false);
    let root = crate::plan_save::workspace_root(state, &thread_id);
    let can_save = crate::plan_save::may_write(state) && root.is_some();
    let markdown = item["markdown"].as_str().unwrap_or_default().to_owned();
    let title = words::plan_title(&markdown).unwrap_or_else(|| "Proposed plan".into());
    let collapsible = words::plan_can_collapse(&markdown) && item["streaming"] != true;
    let shown = if collapsible && !expanded() { words::plan_preview(&markdown, 10) } else { words::plan_body(&markdown) };
    let status = (item["status"] != "completed").then(|| item["status"].as_str().unwrap_or_default().to_owned());
    rsx! {section {class:"plan-card","data-item-type":"proposed_plan","aria-label":"Proposed plan",
        header {class:"plan-header",
            span {class:"badge","Plan"}h3 {"{title}"}
            if let Some(status)=status {span {class:"activity-status","{status}"}}
            button {"aria-label":"Plan actions","aria-haspopup":"menu","aria-expanded":"{actions()}",class:"icon-button",onclick:move |_|actions.set(!actions()),"⋯"}
        }
        if actions() {div {class:"menu",role:"menu",onkeydown:move |event| {if event.key()==Key::Escape {actions.set(false);}},
            button {role:"menuitem",onclick:{let text=words::plan_export(&markdown);move |_|{on_copy.call(text.clone());actions.set(false);}},"Copy to clipboard"}
            button {role:"menuitem",onclick:{let text=words::plan_export(&markdown);let name=words::plan_filename(&markdown);move |_|{on_download.call((name.clone(),text.clone()));actions.set(false);}},"Download as markdown"}
            button {role:"menuitem","aria-label":"Save to workspace",disabled:!can_save,title:if can_save {""} else {"This connection cannot write files, or the thread has no workspace."},onclick:move |_|{actions.set(false);saving.set(true);},"Save to workspace"}
        }}
        if saving() {if let Some(root)=root.clone() {crate::plan_save::SaveDialog {state,transport:transport.clone(),root,filename:words::plan_filename(&markdown),contents:words::plan_export(&markdown),close:move |_|saving.set(false)}}}
        Markdown {text:shown}
        if collapsible {button {class:"plan-toggle",onclick:move |_|expanded.set(!expanded()),if expanded() {"Collapse plan"} else {"Expand plan"}}}
    }}
}

/// The plan's steps. In progress shows only while the run that owns it is going.
#[component]
pub(crate) fn Todo(item: Value, live: Live) -> Element {
    let steps: Vec<_> = item["steps"].as_array().into_iter().flatten().map(|step| (step["id"].as_str().unwrap_or_default().to_owned(), step.clone())).collect();
    let done = steps.iter().filter(|(_, step)| step["status"] == "completed").count();
    rsx! {section {class:"todo-card","aria-label":"Plan updated","data-item-type":"todo_list",
        header {class:"todo-header",strong {"Plan updated"}small {"{done}/{steps.len()}"}}
        if let Some(explanation)=item["explanation"].as_str().filter(|text|!text.trim().is_empty()) {p {"{explanation}"}}
        ol {for (id,step) in steps {{
            let status=step["status"].as_str().unwrap_or("pending");
            let shown=if status=="running"{if live==Live::Yes{"running"}else{"pending"}}else{status};
            rsx!{li {key:"{id}",class:if shown=="completed"{"completed"}else{""},
                span {class:"step-status","aria-label":"{shown}",if shown=="completed"{"✓"}else if shown=="running"{"◉"}else{"○"}}
                span {{step["text"].as_str().unwrap_or_default()}}
                if let Some(duration)=step["durationMs"].as_u64() {small {"{duration / 1000}s"}}
            }}
        }}}
    }}
}

/// The body of an expanded tool-like row (`V2ItemInspector`).
pub(crate) fn body_text(item: &Value) -> Option<String> {
    match item["type"].as_str()? {
        "approval_request" => item["prompt"].as_str().map(str::to_owned),
        "user_input_request" => Some(item["questions"].as_array()?.iter().filter_map(|question| question["question"].as_str()).collect::<Vec<_>>().join("\n\n")).filter(|text| !text.is_empty()),
        _ => work_log::output_text(item),
    }
}

/// A subagent's status the way the original words it: every state reads, finished ones too.
pub(crate) fn subagent_status(item: &Value) -> &'static str {
    match item["status"].as_str().unwrap_or_default() {
        "pending" => "Queued",
        "running" => "Running",
        "waiting" => "Waiting",
        "idle" => "Idle · resumable",
        "completed" => "Completed",
        "failed" => "Failed",
        "cancelled" | "interrupted" => "Stopped",
        _ => "",
    }
}
