use crate::{
    Message,
    markdown::Markdown,
    runtime::{self, UiModel},
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
) -> Element {
    let mut detail = use_signal(|| None::<(String, Value)>);
    let mut loading = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<(String, String)>);
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
    if matches!(kind, "assistant_message" | "user_message") {
        return rsx! {Message {role:(if kind=="user_message"{"user"}else{"assistant"}).to_owned(),text:effective["text"].as_str().unwrap_or_default().to_owned()}};
    }
    if kind == "proposed_plan" {
        return rsx! {section {class:"plan-card",h3 {"Proposed plan"}Markdown {text:effective["markdown"].as_str().unwrap_or_default().to_owned()}}};
    }
    if kind == "todo_list" {
        let steps: Vec<_> = effective["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|step| (step["id"].as_str().unwrap_or_default().to_owned(), step))
            .collect();
        return rsx! {section {class:"todo-card","aria-label":"Plan progress",
            if let Some(explanation)=effective["explanation"].as_str(){p {"{explanation}"}}
            ol {for (id,step) in steps {li {key:"{id}",class:if step["status"]=="completed"{"completed"}else{""},
                span {class:"step-status","aria-label":step["status"].as_str().unwrap_or("pending"),if step["status"]=="completed"{"✓"}else if step["status"]=="running"{"◉"}else{"○"}}
                span {{step["text"].as_str().unwrap_or_default()}}
                if let Some(duration)=step["durationMs"].as_u64(){small {"{duration / 1000}s"}}
            }}}
        }};
    }
    if matches!(
        kind,
        "system_notice"
            | "run_interrupt_request"
            | "run_interrupt_result"
            | "notification"
            | "error"
    ) {
        let message = if kind == "error" {
            effective["failure"]["message"].as_str()
        } else if kind == "notification" {
            effective["detail"]
                .as_str()
                .or_else(|| effective["title"].as_str())
        } else {
            effective["message"].as_str()
        }
        .unwrap_or_default();
        return rsx! {aside {class:if kind=="error"{"activity-notice failure"}else{"activity-notice"},role:if kind=="error"{"alert"}else{"note"},"{message}",
            if kind=="error" {if let Some(retry)=effective.get("retry"){p {class:"muted",{if retry["willRetry"]==true{"Retrying…"}else{"Retry stopped"}}}}}
        }};
    }
    let label = match kind {
        "reasoning" => "Thinking".to_owned(),
        "command_execution" => effective["input"]
            .as_str()
            .unwrap_or("Command")
            .lines()
            .next()
            .unwrap_or("Command")
            .to_owned(),
        "file_change" => effective["fileName"]
            .as_str()
            .unwrap_or("File changed")
            .to_owned(),
        "file_search" => "File search".into(),
        "web_search" => "Web search".into(),
        "dynamic_tool" => effective["toolName"]
            .as_str()
            .unwrap_or("Tool call")
            .to_owned(),
        "approval_request" => "Approval request".into(),
        "user_input_request" => "User input".into(),
        "checkpoint" => "Checkpoint".into(),
        "compaction" => "Context compacted".into(),
        "handoff" => "Model handoff".into(),
        "fork" => "Thread forked".into(),
        "thread_created" => "Thread created".into(),
        "subagent" => "Subagent".into(),
        "secret_request" => effective["label"]
            .as_str()
            .unwrap_or("Secret requested")
            .to_owned(),
        _ => return rsx! {},
    };
    let output = work_log::output_text(&effective);
    let omitted = work_log::needs_detail(&effective);
    let status = effective["status"].as_str().unwrap_or_default();
    let additions = effective["additions"].as_u64().unwrap_or(0);
    let deletions = effective["deletions"].as_u64().unwrap_or(0);
    let request_item = effective.clone();
    let revision_for_click = revision.clone();
    let cache_key_for_click = cache_key.clone();
    rsx! {
        details {class:"tool-activity", "data-item-type":kind,
            summary {span {class:"activity-title","{label}"}span {class:if status=="failed"{"activity-status failure"}else{"activity-status"},"{status}"}
                if kind=="file_change" {span {class:"diff-count additions", "+{additions}"}span {class:"diff-count deletions","−{deletions}"}}
            }
            div {class:"activity-body",
                match kind {
                    "reasoning"=>rsx!{Markdown {text:effective["text"].as_str().unwrap_or_default().to_owned()}},
                    "command_execution"=>rsx!{pre {class:"command-input",{effective["input"].as_str().unwrap_or_default()}}if let Some(code)=effective["exitCode"].as_i64(){p {class:if code==0{"muted"}else{"failure"},"Exit code {code}"}}},
                    "file_change"=>rsx!{FileChange {item:effective.clone()}},
                    "dynamic_tool"=>rsx!{if let Some(input)=work_log::format_value(&effective["input"]){pre {class:"tool-input","{input}"}}},
                    "file_search"=>rsx!{if let Some(pattern)=effective["pattern"].as_str(){pre {"{pattern}"}}},
                    "web_search"=>rsx!{for pattern in effective["patterns"].as_array().into_iter().flatten(){p {{pattern.as_str().unwrap_or_default()}}}
                        for result in effective["results"].as_array().into_iter().flatten(){SearchResult {result:result.clone()}}
                    },
                    "approval_request"=>rsx!{if let Some(prompt)=effective["prompt"].as_str(){pre {"{prompt}"}}},
                    "user_input_request"=>rsx!{for question in effective["questions"].as_array().into_iter().flatten(){p {{question["question"].as_str().unwrap_or_default()}}}},
                    "checkpoint"=>rsx!{ul {for file in effective["files"].as_array().into_iter().flatten(){li {{file["path"].as_str().or_else(||file["fileName"].as_str()).unwrap_or_default()}}}}},
                    "compaction"|"handoff"=>rsx!{if let Some(summary)=effective["summary"].as_str(){pre {"{summary}"}}},
                    "subagent"=>rsx!{p {{effective["prompt"].as_str().unwrap_or_default()}}if let Some(progress)=effective["progress"].as_str(){pre {"{progress}"}}if let Some(result)=effective["result"].as_str(){pre {"{result}"}}},
                    "secret_request"=>rsx!{p {{effective["reason"].as_str().unwrap_or_default()}}p {class:"muted",{effective["secretStatus"].as_str().unwrap_or_default()}}},
                    _=>rsx!{},
                }
                if let Some(output)=output.filter(|_|kind!="web_search"){pre {class:"tool-output","{output}"}}
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
                if let Some(target)=effective["childThreadId"].as_str().or_else(||effective["targetThreadId"].as_str()) {button {onclick:{let transport=transport.clone();let target=target.to_owned();move |_|runtime::select_thread(&transport,state,target.clone())},"Open thread"}}
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
