//! The conversation as rows: a turn's tool activity gathered under one summary line, settled turns
//! and superseded attempts folded behind a line of their own, and the running turn's live line.
//! The rules are `t3_client::timeline_rows`; this draws them. Every row is a direct child of the
//! timeline with a `data-timeline-row` of its own, which the scroll code measures. A summary line
//! takes plain props, so a tool call streaming inside a group repaints that call alone, and the
//! line itself only when its words change. Provider- and tool-supplied text is drawn as text,
//! and a tool's icon only as an image of an http(s) or data URL.
use crate::{
    composer_panels, item_cards,
    runtime::{self, UiModel},
    timeline::TimelineItem,
};
use dioxus::prelude::*;
use serde_json::Value;
use std::collections::BTreeSet;
use t3_client::{
    rollback::Target,
    timeline_items as words,
    timeline_rows::{Entry, GroupAction, Row, SummaryKind, ToolIcon},
};

/// Which folds and groups the reader has opened. They belong to the open conversation and are gone
/// with it, as in the original.
#[derive(Clone, Copy)]
pub(crate) struct Disclosures {
    pub runs: Signal<BTreeSet<String>>,
    pub attempts: Signal<BTreeSet<String>>,
    pub groups: Signal<BTreeSet<String>>,
}
impl Disclosures {
    pub(crate) fn new() -> Self {
        Self { runs: use_signal(BTreeSet::new), attempts: use_signal(BTreeSet::new), groups: use_signal(BTreeSet::new) }
    }
}

fn toggle(mut set: Signal<BTreeSet<String>>, id: &str) {
    set.with_mut(|set| {
        if !set.remove(id) {
            set.insert(id.to_owned());
        }
    });
}

/// A small mark for what a group did. The surface a tool used (a browser, a computer) takes
/// precedence, as in the original.
fn glyph(kind: SummaryKind, surface: Option<&str>) -> &'static str {
    match surface {
        Some("browser") => return "◍",
        Some("computer") => return "▢",
        _ => {}
    }
    match kind {
        SummaryKind::Action(GroupAction::Read) => "▤",
        SummaryKind::Action(GroupAction::Edit) => "✎",
        SummaryKind::Action(GroupAction::Command) => "›_",
        SummaryKind::Action(GroupAction::ThreadCreate) => "＋",
        SummaryKind::Action(GroupAction::Browser) => "◍",
        SummaryKind::Action(GroupAction::Device) => "▢",
        SummaryKind::Action(GroupAction::Search | GroupAction::CodeSearch) => "⌕",
        SummaryKind::Action(GroupAction::Other) => "⚙",
        SummaryKind::Reasoning => "…",
        SummaryKind::AgentTool => "◈",
        SummaryKind::ToneTool => "ϟ",
        SummaryKind::DynamicTool | SummaryKind::Action(GroupAction::Update) | SummaryKind::Other | SummaryKind::Mixed => "⚒",
    }
}

/// The picture a tool brought, or the group's mark.
#[component]
fn Mark(glyph: String, icon: Option<ToolIcon>, failed: bool) -> Element {
    let image = match &icon {
        Some(ToolIcon::Website { favicon: Some(url), .. }) => Some((url.clone(), None)),
        Some(ToolIcon::ThemedLogo { logo, logo_dark }) => Some((logo.clone(), logo_dark.clone())),
        _ => None,
    };
    rsx! {
        span {class:if failed {"work-mark failed"} else {"work-mark"},"aria-hidden":"true",
            if let Some((light, dark)) = image {
                picture {
                    if let Some(dark) = dark {source {"media":"(prefers-color-scheme: dark)","srcset":"{dark}"}}
                    img {src:"{light}",alt:"",loading:"lazy",width:"16",height:"16"}
                }
            } else {"{glyph}"}
        }
    }
}

/// "Worked for 2m 4s" above a settled turn, or "Superseded attempt" above partial output.
#[component]
fn FoldRow(attempt: bool, id: String, label: String, expanded: bool, set: Signal<BTreeSet<String>>) -> Element {
    rsx! {button {class:if attempt {"attempt-fold"} else {"turn-fold"},r#type:"button","aria-label":"{label}","aria-expanded":"{expanded}","data-scroll-anchor-ignore":"true",
        onclick:move |_| toggle(set, &id),
        span {class:"fold-label","{label}"}
        span {class:"fold-chevron","aria-hidden":"true",if expanded {"▾"} else {"▸"}}
        if attempt {span {class:"muted","Partial output retained"}}
    }}
}

#[component]
fn WorkSummary(group: String, summary: String, count: usize, glyph: String, icon: Option<ToolIcon>, failed: bool, expanded: bool, set: Signal<BTreeSet<String>>) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.summary.set(probe.summary.get() + 1);
    }
    let name = if failed { format!("{summary}, tool call failed") } else { summary.clone() };
    rsx! {button {class:"work-summary",r#type:"button","aria-label":"{name}","aria-expanded":"{expanded}","data-scroll-anchor-ignore":"true",title:"{count} in this group",
        onclick:move |_| toggle(set, &group),
        Mark {glyph,icon,failed}
        span {class:"work-label","{summary}"}
        span {class:"fold-chevron","aria-hidden":"true",if expanded {"▾"} else {"▸"}}
    }}
}

/// The running turn's line: the call in progress, or the latest finished one in the past tense,
/// with the latest thought above it. Nothing animates; the text is the state.
#[component]
fn LiveRow(group: String, label: String, thought: Option<String>, active: bool, failed: bool, glyph: String, icon: Option<ToolIcon>, expanded: bool, set: Signal<BTreeSet<String>>) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.summary.set(probe.summary.get() + 1);
    }
    let name = if failed { format!("{label}, tool call failed") } else { label.clone() };
    rsx! {button {class:if active {"live-activity active"} else {"live-activity"},r#type:"button","aria-label":"{name}","aria-expanded":"{expanded}","data-scroll-anchor-ignore":"true",
        onclick:move |_| toggle(set, &group),
        if let Some(thought) = thought.filter(|_| !expanded) {span {class:"live-thought","{thought}"}}
        span {class:"live-line",Mark {glyph,icon,failed}span {class:"work-label","{label}"}}
    }}
}

/// "Thinking" while a running turn has nothing else to show. After a failed call it opens the
/// group that failed.
#[component]
fn ThinkingRow(group: Option<String>, expanded: bool, set: Signal<BTreeSet<String>>) -> Element {
    match group {
        Some(group) => rsx! {button {class:"live-activity active",r#type:"button","aria-expanded":"{expanded}","data-scroll-anchor-ignore":"true",onclick:move |_| toggle(set, &group),
            span {class:"live-line",Mark {glyph:"…".to_owned(),icon:None,failed:false}span {class:"work-label","Thinking"}}}},
        None => rsx! {div {class:"live-activity active",role:"status",span {class:"live-line",Mark {glyph:"…".to_owned(),icon:None,failed:false}span {class:"work-label","Thinking"}}}},
    }
}

/// Everything a row needs from the open conversation.
pub(crate) struct Rows<'a> {
    pub state: Store<UiModel>,
    pub transport: runtime::TransportHandle,
    pub environment_key: &'a str,
    pub thread_id: &'a str,
    pub projection: &'a Value,
    pub items: &'a [Value],
    pub entries: &'a [Entry],
    pub offers: &'a std::collections::BTreeMap<String, Target>,
    pub active_run_id: Option<&'a str>,
    pub working_since: Option<&'a str>,
    pub disclosures: Disclosures,
    pub edit_target: fn(&std::collections::BTreeMap<String, Target>, &str, &Value) -> Option<Target>,
}

impl Rows<'_> {
    fn key(&self, row: &Value) -> String {
        serde_json::to_string(&(self.environment_key, &row["sourceThreadId"], &row["sourceItemId"])).expect("scoped item identity")
    }
    /// One turn item as its own card, wherever it sits.
    fn item(&self, index: usize, prefix: &str) -> Element {
        let row = &self.items[index];
        let key = self.key(row);
        let marker = format!("{prefix}{key}");
        // A report about a subagent the parent knows is that subagent's row, with the status the report carried.
        if row["item"]["type"] == "notification" {
            let at = row["item"]["startedAt"].as_str().or_else(|| row["item"]["updatedAt"].as_str()).unwrap_or_default();
            if let Some(card) = crate::subagent_rows::notification_card(self.state, self.projection, &row["item"], at) {
                return rsx! { div { key:"{marker}","data-timeline-row":"{marker}",class:"subagent-batch",crate::subagent_rows::SubagentRow {card,state:self.state,transport:self.transport.clone()}} };
            }
        }
        rsx! { div { key:"{marker}","data-timeline-row":"{marker}","data-message-id":row["item"]["messageId"].as_str().unwrap_or_default(),
            TimelineItem { environment_key:self.environment_key.to_owned(),item:row["item"].clone(),state:self.state,transport:self.transport.clone(),
                source_thread_id:row["sourceThreadId"].as_str().unwrap_or(self.thread_id).to_owned(),source_item_id:row["sourceItemId"].as_str().unwrap_or_default().to_owned(),
                rewind:(self.edit_target)(self.offers,self.thread_id,row),
                active_run:if item_cards::in_progress(&row["item"]) { self.active_run_id.map(str::to_owned) } else { None } }
        }}
    }
    fn members(&self, group: &str, members: &[usize]) -> Element {
        let marker = format!("members:{group}");
        rsx! { div { key:"{marker}",class:"work-members","data-timeline-row":"{marker}",
            for &member in members {{ let row = &self.items[self.entries[member].row]; let key = self.key(row); rsx!{ div { key:"{key}",class:"work-member",
                TimelineItem { environment_key:self.environment_key.to_owned(),item:row["item"].clone(),state:self.state,transport:self.transport.clone(),
                    source_thread_id:row["sourceThreadId"].as_str().unwrap_or(self.thread_id).to_owned(),source_item_id:row["sourceItemId"].as_str().unwrap_or_default().to_owned(),
                    rewind:None,active_run:if item_cards::in_progress(&row["item"]) { self.active_run_id.map(str::to_owned) } else { None } }
            }} }}
        }}
    }

    pub(crate) fn render(&self, rows: &[Row]) -> Vec<Element> {
        let Disclosures { runs, attempts, groups } = self.disclosures;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let element = match row {
                Row::Entry { entry } | Row::Single { entry, .. } | Row::Compaction { entry, .. } => self.item(self.entries[*entry].row, ""),
                Row::Item { row } => self.item(*row, ""),
                Row::Created { entry } => self.item(self.entries[*entry].row, "summary:"),
                Row::Footer { .. } => continue,
                Row::Subagents { entries } => {
                    let first = self.items[self.entries[entries[0]].row]["sourceItemId"].as_str().unwrap_or_default().to_owned();
                    let marker = format!("subagents:{first}");
                    let items: Vec<&Value> = entries.iter().map(|&entry| &self.items[self.entries[entry].row]["item"]).collect();
                    let cards = crate::subagent_rows::cards(self.state, self.projection, &items);
                    rsx! { div { key:"{marker}",class:"subagent-batch","data-timeline-row":"{marker}",
                        if cards.len() == 1 {
                            crate::subagent_rows::SubagentRow {card:cards[0].clone(),state:self.state,transport:self.transport.clone()}
                        } else {
                            crate::subagent_rows::SubagentGroup {id:marker.clone(),cards,state:self.state,transport:self.transport.clone()}
                        }
                    }}
                }
                Row::TurnFold { run_id, label, expanded, .. } => {
                    let marker = format!("turn-fold:{run_id}");
                    rsx! { div { key:"{marker}",class:"fold-row","data-timeline-row":"{marker}",FoldRow {attempt:false,id:run_id.clone(),label:label.clone(),expanded:*expanded,set:runs}} }
                }
                Row::AttemptFold { attempt_id, label, expanded, .. } => {
                    let marker = format!("attempt-fold:{attempt_id}");
                    rsx! { div { key:"{marker}",class:"fold-row","data-timeline-row":"{marker}",FoldRow {attempt:true,id:attempt_id.clone(),label:label.clone(),expanded:*expanded,set:attempts}} }
                }
                Row::Toggle { id, group, entries, summary, kind, surface, icon, has_failure, expanded, .. } => rsx! { div { key:"{id}",class:"work-row","data-timeline-row":"{id}",
                    WorkSummary {group:group.clone(),summary:summary.clone(),count:entries.len(),glyph:glyph(*kind,surface.as_deref()).to_owned(),icon:icon.clone(),failed:*has_failure,expanded:*expanded,set:groups}
                }},
                Row::Live { id, group, entry, label, expanded, active, thought, surface, icon, failed, .. } => {
                    let text_of = |index: usize| self.items[self.entries[index].row]["item"]["text"].as_str().unwrap_or_default();
                    let thought_line = thought.map(|index| words::live_thought_line(text_of(index))).filter(|line| !line.is_empty());
                    // While the thought itself is the line, it says Thinking (or Thought) and the thought sits above it.
                    let is_thought = *thought == Some(*entry) && thought_line.is_some() && !*expanded;
                    let shown = if is_thought { (if *active { "Thinking" } else { "Thought" }).to_owned() }
                        else if self.items[self.entries[*entry].row]["item"]["type"] == "reasoning" { words::live_thought_line(text_of(*entry)) }
                        else { label.clone() };
                    rsx! { div { key:"{id}",class:"work-row","data-timeline-row":"{id}",
                        LiveRow {group:group.clone(),label:shown,thought:thought_line,active:*active,failed:*failed,glyph:glyph(SummaryKind::Mixed,surface.as_deref()).to_owned(),icon:icon.clone(),expanded:*expanded,set:groups}
                    }}
                }
                Row::Members { group, entries } => self.members(group, entries),
                Row::Working { since } => rsx! { composer_panels::WorkingRow { key:"working:{self.thread_id}", started: since.clone().or_else(|| self.working_since.map(str::to_owned)) } },
                Row::Thinking { group, expanded, .. } => {
                    let marker = format!("thinking:{}", group.as_deref().unwrap_or("live"));
                    rsx! { div { key:"{marker}",class:"work-row","data-timeline-row":"{marker}",ThinkingRow {group:group.clone(),expanded:*expanded,set:groups}} }
                }
            };
            out.push(element);
        }
        out
    }
}

/// When a provider's own subagent thread (it has no run) started its current work, if it is working.
pub(crate) fn runless_work_started_at(projection: &Value) -> Option<String> {
    let thread = &projection["thread"];
    if thread["lineage"]["relationshipToParent"] != "subagent" {
        return None;
    }
    let node = projection["nodes"].as_array()?.iter().rev().find(|node| node["kind"] == "root_turn" && node["runId"].is_null())?;
    matches!(node["status"].as_str(), Some("pending" | "running" | "waiting")).then(|| node["startedAt"].as_str().unwrap_or_default().to_owned())
}
