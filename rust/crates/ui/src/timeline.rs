use crate::{
    Message,
    markdown::Markdown,
    runtime::{self, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::work_log;

#[component]
pub(crate) fn TimelineItem(
    item: Value,
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    environment_key: String,
    source_thread_id: String,
    source_item_id: String,
    /// Where "Edit from here" rewinds to, when this message's turn can be rewound.
    rewind: Option<t3_client::rollback::Target>,
    /// The run still going, given only to rows whose work is in progress (so a run starting does not rebuild finished rows).
    active_run: Option<String>,
    /// `local`, or `inherited` for a row a fork carries from its source thread.
    #[props(default = "local".to_owned())]
    visibility: String,
) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.item.set(probe.item.get() + 1);
    }
    let mut detail = use_signal(|| None::<(String, Value)>);
    let mut loading = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<(String, String)>);
    let mut open = use_signal(|| false);
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<DetailProbe>() {
        let environment = environment_key.clone();
        use_hook(move || {
            probe.0.borrow_mut().push((environment, detail));
        });
    }
    let revision = work_log::detail_revision(&item);
    let cache_key = serde_json::to_string(&(
        &environment_key,
        &source_thread_id,
        &source_item_id,
        &revision,
    ))
    .expect("scoped detail identity");
    let effective = detail
        .read()
        .as_ref()
        .filter(|(key, _)| key == &cache_key)
        .map(|(_, detail)| work_log::overlay_detail(&item, detail))
        .unwrap_or_else(|| item.clone());
    let kind = effective["type"].as_str().unwrap_or("");
    let attachments = effective["attachments"].as_array().filter(|list| kind == "user_message" && !list.is_empty());
    let edit = rewind.filter(|_| kind == "user_message").and_then(|target| {
        Some(rsx! {crate::rollback::EditFromHere {state,environment_key:environment_key.clone(),thread_id:source_thread_id.clone(),run_id:effective["runId"].as_str()?.to_owned(),
            text:effective["text"].as_str().unwrap_or_default().to_owned(),attachments:effective["attachments"].as_array().cloned().unwrap_or_default(),target}})
    });
    if let Some(attachments) = attachments {
        let presented = t3_client::user_message::present(&effective);
        return rsx! {article {class:"message user",
            Attribution {attribution:presented.attribution.clone(),state,transport:transport.clone()}
            if !presented.text.is_empty() {Markdown {text:presented.text.clone()}}
            MessageAttachments {environment_key:environment_key.clone(),state,transport:transport.clone(),attachments:attachments.clone()}
            {edit}
        }};
    }
    if kind == "user_message" {
        let presented = t3_client::user_message::present(&effective);
        return rsx! {article {class:"message user",
            Attribution {attribution:presented.attribution,state,transport:transport.clone()}
            Markdown {text:presented.text}
            {edit}
        }};
    }
    if kind == "assistant_message" {
        return rsx! {Message {role:"assistant".to_owned(),text:effective["text"].as_str().unwrap_or_default().to_owned(),{edit}}};
    }
    let live = crate::item_cards::liveness(&effective, active_run.as_deref());
    match kind {
        "proposed_plan" => return rsx! {crate::item_cards::PlanCard {item:effective.clone(),state,transport:transport.clone(),thread_id:source_thread_id.clone(),on_copy:|text|copy_text(text),on_download:|(name,text)|download_text(name,text)}},
        "todo_list" => return rsx! {crate::item_cards::Todo {item:effective.clone(),live}},
        "reasoning" => return rsx! {crate::item_cards::Reasoning {item:effective.clone(),live}},
        "compaction" => return rsx! {crate::item_cards::Compaction {item:effective.clone()}},
        "error" => return rsx! {crate::item_cards::ErrorRow {item:effective.clone()}},
        "run_interrupt_request" | "run_interrupt_result" => return rsx! {crate::item_cards::RunNotice {item:effective.clone()}},
        "secret_request" => return rsx! {crate::secret_card::SecretRequestCard {item:effective.clone(),visibility:visibility.clone(),state,transport:transport.clone()}},
        "notification" => return rsx! {crate::item_cards::Notification {item:effective.clone(),state,transport:transport.clone()}},
        "system_notice" => {
            let message = effective["message"].as_str().or_else(|| effective["detail"].as_str()).or_else(|| effective["title"].as_str()).unwrap_or_default().to_owned();
            return rsx! {aside {class:"activity-notice warning",role:"note","data-item-type":kind,span {class:"work-mark","aria-hidden":"true","!"}"{message}"}};
        }
        _ => {}
    }
    if kind == "checkpoint" {
        return rsx! {crate::changed_files::ChangedFiles {state,environment_key:environment_key.clone(),thread_id:source_thread_id.clone(),item:effective.clone()}};
    }
    if !matches!(kind, "command_execution" | "file_change" | "file_search" | "web_search" | "dynamic_tool" | "approval_request" | "user_input_request" | "handoff" | "fork" | "thread_created" | "subagent" | "secret_request") {
        return rsx! {};
    }
    let label = crate::item_cards::label(&effective);
    let output = work_log::output_text(&effective);
    let omitted = work_log::needs_detail(&effective);
    let status = if kind == "subagent" { crate::item_cards::subagent_status(&effective) } else { crate::item_cards::status_word(&effective, live).unwrap_or_default() };
    let additions = effective["additions"].as_u64().unwrap_or(0);
    let deletions = effective["deletions"].as_u64().unwrap_or(0);
    let request_item = effective.clone();
    let revision_for_click = revision.clone();
    let cache_key_for_click = cache_key.clone();
    rsx! {
        details {class:"tool-activity", "data-item-type":kind,open:open(),
            summary {"aria-label":"{label}","aria-expanded":"{open()}",onclick:move |event| {event.prevent_default();let now=*open.peek();open.set(!now);},span {class:"activity-title","{label}"}if !status.is_empty() {span {class:if status=="failed"{"activity-status failure"}else{"activity-status"},"{status}"}}
                if kind=="file_change" {span {class:"diff-count additions", "+{additions}"}span {class:"diff-count deletions","−{deletions}"}}
            }
            if open() {div {class:"activity-body",
                match kind {
                                        "command_execution"=>rsx!{pre {class:"command-input",{effective["input"].as_str().unwrap_or_default()}}if let Some(code)=effective["exitCode"].as_i64(){p {class:if code==0{"muted"}else{"failure"},"Exit code {code}"}}},
                    "file_change"=>rsx!{FileChange {item:effective.clone()}},
                    "dynamic_tool"=>rsx!{if let Some(input)=work_log::format_value(&effective["input"]){pre {class:"tool-input","{input}"}}},
                    "file_search"=>rsx!{if let Some(pattern)=effective["pattern"].as_str(){pre {"{pattern}"}}},
                    "web_search"=>rsx!{for pattern in effective["patterns"].as_array().into_iter().flatten(){p {{pattern.as_str().unwrap_or_default()}}}
                        for result in effective["results"].as_array().into_iter().flatten(){SearchResult {result:result.clone()}}
                    },
                    "approval_request"=>rsx!{if let Some(prompt)=effective["prompt"].as_str(){pre {class:"approval-prompt","{prompt}"}}},
                    "user_input_request"=>rsx!{AnsweredQuestions {item:effective.clone()}},
                    "handoff"=>rsx!{if let Some(summary)=effective["summary"].as_str(){pre {"{summary}"}}},
                    "subagent"=>rsx!{p {{effective["prompt"].as_str().unwrap_or_default()}}if let Some(progress)=effective["progress"].as_str(){pre {"{progress}"}}if let Some(result)=effective["result"].as_str(){pre {"{result}"}}},
                    "secret_request"=>rsx!{p {{effective["reason"].as_str().unwrap_or_default()}}p {class:"muted",{effective["secretStatus"].as_str().unwrap_or_default()}}},
                    _=>rsx!{},
                }
                if let Some(output)=output.filter(|_|kind!="web_search"){pre {class:"tool-output","{output}"}}
                if kind=="dynamic_tool" && !omitted {
                    // The images the tool returned, numbered as the server numbers them; the thread and item are the item's own (a fork's inherited rows belong to their source).
                    for index in 0..work_log::output_image_count(&effective["output"]) {
                        crate::file_preview::ToolOutputImage {key:"{effective[\"id\"]}:{index}",state,transport:transport.clone(),environment_key:environment_key.clone(),thread_id:effective["threadId"].as_str().unwrap_or(&source_thread_id).to_owned(),item_id:effective["id"].as_str().unwrap_or(&source_item_id).to_owned(),index}
                    }
                }
                if let Some(path)=crate::file_preview::viewed_path(&effective) {crate::file_preview::WorkspaceImage {key:"{path}",state,transport:transport.clone(),environment_key:environment_key.clone(),thread_id:source_thread_id.clone(),path}}
                if omitted {
                    button {"aria-label":"Load full output",disabled:loading.read().as_ref()==Some(&cache_key),onclick:{let transport=transport.clone();move |_|{
                        if loading.peek().as_ref()==Some(&cache_key_for_click){return;}
                        let revision=revision_for_click.clone();let key=cache_key_for_click.clone();loading.set(Some(key.clone()));error.set(None);
                        let owner=runtime::response_owner(&transport,state);
                        let transport=transport.clone();let thread_id=source_thread_id.clone();let item_id=source_item_id.clone();let projected=request_item.clone();
                        spawn(async move {
                            let result=runtime::request_value(transport.clone(),state,"orchestration.getTurnItem",json!({"threadId":thread_id,"itemId":item_id,"revision":revision})).await.and_then(|value|serde_json::from_value::<t3_contracts::GetTurnItemResult>(value).and_then(serde_json::to_value).map_err(|error|format!("Invalid full output: {error}")));
                            if owner.is_none() || owner!=runtime::response_owner(&transport,state){return;}
                            if loading.peek().as_ref()!=Some(&key){return;}loading.set(None);
                            match result {
                                Ok(value) if value["item"].is_object() && value["item"]["id"]==projected["id"] && value["item"]["type"]==projected["type"]=>detail.set(Some((key,value["item"].clone()))),
                                Ok(_)=>error.set(Some((key,"Full output is no longer available.".into()))),
                                Err(message)=>error.set(Some((key,message))),
                            }
                        });
                    }},if loading.read().as_ref()==Some(&cache_key){"Loading full output…"}else{"Load full output"}}
                }
                if let Some((key,message))=&*error.read(){if key==&cache_key{p {class:"failure",role:"alert","{message}"}}}
                if let Some(target)=effective["childThreadId"].as_str().or_else(||effective["targetThreadId"].as_str()) {button {onclick:{let transport=transport.clone();let target=target.to_owned();move |_|runtime::select_thread(&transport,state,target.clone())},if kind=="subagent" {"Open subagent thread"} else {"Open thread"}}}
            }
            }
        }
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct DetailProbe(
    pub std::rc::Rc<std::cell::RefCell<Vec<(String, Signal<Option<(String, Value)>>)>>>,
);

#[component]
fn SearchResult(result: Value) -> Element {
    let url = result["url"].as_str().unwrap_or_default();
    let title = result["title"].as_str().unwrap_or(url);
    let safe = url.trim().to_ascii_lowercase();
    rsx! {article {class:"search-result",
        if safe.starts_with("https://") || safe.starts_with("http://"){a {href:url,target:"_blank",rel:"noreferrer","{title}"}}else{p {"{title}"}}
        if let Some(snippet)=result["snippet"].as_str(){p {"{snippet}"}}
    }}
}

#[component]
fn FileChange(item: Value) -> Element {
    let diff = item["diffStr"].as_str();
    let operations: Vec<_> = item["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|change| {
            format!(
                "{} {}{}",
                change["operation"].as_str().unwrap_or_default(),
                change["oldPath"]
                    .as_str()
                    .map(|old| format!("{old} → "))
                    .unwrap_or_default(),
                change["path"].as_str().unwrap_or_default()
            )
        })
        .collect();
    rsx! {div {class:"file-change",
        for operation in operations {p {class:"file-operation","{operation}"}}
        if let Some(diff)=diff {pre {class:"unified-diff",for (index,line) in diff.lines().enumerate(){div {key:"{index}",class:if line.starts_with('+')&&!line.starts_with("+++"){"additions"}else if line.starts_with('-')&&!line.starts_with("---"){"deletions"}else{""},"{line}\n"}}}}
        else {if let Some(old)=item["oldStr"].as_str(){h4 {"Before"}pre {"{old}"}}if let Some(new)=item["newStr"].as_str(){h4 {"After"}pre {"{new}"}}}
    }}
}

/// Sent attachments: image thumbnails in a fixed-aspect box, so the timeline does not
/// move when pictures load, and download links for everything else.
#[component]
fn MessageAttachments(
    environment_key: String,
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    attachments: Vec<Value>,
) -> Element {
    let (images, others): (Vec<_>, Vec<_>) = attachments
        .into_iter()
        .map(|attachment| (attachment["id"].as_str().unwrap_or_default().to_owned(), attachment))
        .partition(|(_, attachment)| attachment["type"] == "image");
    rsx! {
        if !images.is_empty() {div {class:"message-images",for (id,attachment) in images {SentAttachment {key:"{id}",environment_key:environment_key.clone(),state,transport:transport.clone(),attachment}}}}
        if !others.is_empty() {div {class:"message-files",for (id,attachment) in others {SentAttachment {key:"{id}",environment_key:environment_key.clone(),state,transport:transport.clone(),attachment}}}}
    }
}

#[component]
fn SentAttachment(
    environment_key: String,
    state: Store<UiModel>,
    transport: runtime::TransportHandle,
    attachment: Value,
) -> Element {
    let mut link = use_signal(|| None::<t3_client::attachments::Preview>);
    let mut asking = use_signal(|| false);
    let (id, name) = (
        attachment["id"].as_str().unwrap_or_default().to_owned(),
        attachment["name"].as_str().unwrap_or("Attachment").to_owned(),
    );
    let resource = json!({"_tag":"attachment","attachmentId":id,"fileName":name,"mimeType":attachment["mimeType"]});
    {
        let (transport, environment_key) = (transport.clone(), environment_key.clone());
        // The URL needs a connection; it is asked for again when one returns.
        use_effect(move || {
            if *state.status().read() != t3_client::connection::ConnectionStatus::Connected
                || link.peek().is_some()
                || *asking.peek()
            {
                return;
            }
            asking.set(true);
            let (transport, environment_key, resource) =
                (transport.clone(), environment_key.clone(), resource.clone());
            spawn(async move {
                if let Ok(preview) =
                    crate::attachments::asset_url(&transport, state, &environment_key, resource).await
                {
                    link.set(Some(preview));
                }
                asking.set(false);
            });
        });
    }
    let size = attachment["sizeBytes"]
        .as_u64()
        .map(t3_client::attachments::format_size)
        .unwrap_or_default();
    if attachment["type"] == "image" {
        return rsx! {
            div {class:"message-image",
                if let Some(preview)=link.read().as_ref() {a {href:"{preview.url}",target:"_blank",rel:"noopener","aria-label":"Open {name}",img {src:"{preview.url}",alt:"{name}",loading:"lazy"}}}
                else {span {class:"message-image-name","{name}"}}
            }
        };
    }
    rsx! {
        div {class:"message-file",
            if let Some(preview)=link.read().as_ref() {a {href:"{preview.url}",download:"{name}",target:"_blank",rel:"noopener","aria-label":"Download {name}",span {"aria-hidden":"true","📄"} span {class:"attachment-name","{name}"} small {"{size}"}}}
            else {span {span {"aria-hidden":"true","📄"} span {class:"attachment-name","{name}"} small {"{size}"}}}
        }
    }
}

/// Puts text on the clipboard from the click that asked for it.
fn copy_text(text: String) {
    let text = serde_json::to_string(&text).expect("clipboard text");
    dioxus::document::eval(&format!("navigator.clipboard?.writeText({text});"));
}
/// Hands the viewer a file to save.
fn download_text(name: String, text: String) {
    let args = serde_json::to_string(&json!([name, text])).expect("download");
    dioxus::document::eval(&format!("const [n,t]={args};const u=URL.createObjectURL(new Blob([t],{{type:'text/markdown;charset=utf-8'}}));const a=document.createElement('a');a.href=u;a.download=n;a.click();setTimeout(()=>URL.revokeObjectURL(u),0);"));
}

/// Who asked what and what was answered, as the original's history of a question shows it.
#[component]
fn AnsweredQuestions(item: Value) -> Element {
    let answer = &item["questionAnswer"];
    let text = |id: &str| answer["answers"][id].as_str().map(str::to_owned).or_else(|| answer["answers"][id].as_array().map(|list| list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
    let rows: Vec<(String, String, Option<String>)> = item["questions"].as_array().into_iter().flatten().map(|question| {
        let id = question["id"].as_str().unwrap_or_default();
        (id.to_owned(), answer["questionTextById"][id].as_str().or_else(|| question["question"].as_str()).unwrap_or_default().to_owned(), text(id).filter(|text| !text.is_empty()))
    }).collect();
    rsx! {for (id,question,reply) in rows {div {key:"{id}",class:"question-history",p {"{question}"}if let Some(reply)=reply {p {class:"question-answer muted","{reply}"}}}}}
}

/// "Sent by automation" or "Sent by another agent" above a message the person did not type; the
/// words link to where it came from when that is known.
#[component]
fn Attribution(attribution: Option<t3_client::user_message::Attribution>, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    use t3_client::user_message::Attribution as Who;
    match attribution {
        None => rsx! {},
        Some(Who::Automation { task_id }) => rsx! {p {class:"message-attribution muted","data-user-message-attribution":"automation",
            if task_id.is_some() {button {class:"link-button",r#type:"button",onclick:move |_|state.view().set(runtime::View::ScheduledTasks),"Sent by automation"}} else {"Sent by automation"}}},
        Some(Who::Agent { sender_thread }) => rsx! {p {class:"message-attribution muted","data-user-message-attribution":"agent",
            if let Some(sender)=sender_thread {button {class:"link-button",r#type:"button","aria-label":"Open sending thread",onclick:move |_|runtime::select_thread(&transport,state,sender.clone()),"Sent by another agent"}} else {"Sent by another agent"}}},
    }
}
