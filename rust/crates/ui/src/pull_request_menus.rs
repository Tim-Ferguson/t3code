//! The reviewer and label menus of the pull request overview. Port of `PullRequestReviewerPicker.tsx`
//! and `PullRequestLabelPicker.tsx` over `PullRequestCandidatePicker.tsx`. The candidates are read
//! fresh each time a menu opens (never held while it is closed) and again after each change, a
//! change is one request whose control waits for the server's answer, and Escape closes the menu.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::rc::Rc;
use t3_client::pull_request::{self as pr, Reference};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Reviewers,
    Labels,
}
impl Kind {
    fn button(self) -> &'static str {
        match self {
            Self::Reviewers => "Request a review",
            Self::Labels => "Change labels",
        }
    }
    fn reason(self) -> &'static str {
        match self {
            Self::Reviewers => "Asking someone to review needs write access on this repository",
            Self::Labels => "Changing labels needs triage access on this repository",
        }
    }
    fn search(self) -> &'static str {
        match self {
            Self::Reviewers => "Search people with access",
            Self::Labels => "Search labels",
        }
    }
    fn read(self) -> &'static str {
        match self {
            Self::Reviewers => "pullRequests.reviewerCandidates",
            Self::Labels => "pullRequests.labelCandidates",
        }
    }
    fn empty(self) -> &'static str {
        match self {
            Self::Reviewers => "Nobody else has access to this repository.",
            Self::Labels => "This repository has no labels.",
        }
    }
    fn no_match(self) -> &'static str {
        match self {
            Self::Reviewers => "Nobody with access matches that.",
            Self::Labels => "No label matches that.",
        }
    }
    fn unreadable(self) -> &'static str {
        match self {
            Self::Reviewers => "The people with access could not be read.",
            Self::Labels => "The labels could not be read.",
        }
    }
    fn truncated(self) -> &'static str {
        match self {
            Self::Reviewers => "This repository has more people with access than are listed here. Ask for the rest on the host.",
            Self::Labels => "This repository has more labels than are listed here. Apply the rest on the host.",
        }
    }
}

/// A candidate as the menu lists it: its name, what to show beside it, whether it is already on,
/// and the payload that turns it on or off.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub name: String,
    pub detail: Option<String>,
    pub on: bool,
    pub request: Value,
}
pub(crate) fn rows(kind: Kind, answer: &Value) -> Vec<Row> {
    answer["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|candidate| match kind {
            Kind::Reviewers => {
                let login = candidate["login"].as_str()?;
                Some(Row { name: login.to_owned(), detail: candidate["name"].as_str().filter(|name| !name.is_empty() && *name != login).map(str::to_owned), on: candidate["isRequested"] == true, request: json!({"reviewers":[{"id":candidate["id"],"kind":candidate["kind"]}]}) })
            }
            Kind::Labels => {
                let name = candidate["name"].as_str()?;
                Some(Row { name: name.to_owned(), detail: candidate["description"].as_str().filter(|text| !text.is_empty()).map(str::to_owned), on: candidate["isApplied"] == true, request: json!({"labels":[name]}) })
            }
        })
        .collect()
}
/// What the toast says for a change, as the original words it.
fn changed(kind: Kind, row: &Row, failed: bool) -> (String, &'static str) {
    match (kind, failed, row.on) {
        (Kind::Reviewers, true, true) => (format!("Could not take back the review request to {}", row.name), "The host refused it. Check that you have write access on this repository, and that they still have access to it."),
        (Kind::Reviewers, true, false) => (format!("Could not ask {} for a review", row.name), "The host refused it. Check that you have write access on this repository, and that they still have access to it."),
        (Kind::Reviewers, false, true) => (format!("Review request to {} taken back", row.name), ""),
        (Kind::Reviewers, false, false) => (format!("Review requested from {}", row.name), ""),
        (Kind::Labels, true, true) => (format!("Could not take {} off", row.name), "The host refused it. Check that you have triage access on this repository."),
        (Kind::Labels, true, false) => (format!("Could not put {} on", row.name), "The host refused it. Check that you have triage access on this repository."),
        (Kind::Labels, false, _) => (String::new(), ""),
    }
}

#[component]
pub(crate) fn Picker(state: Store<UiModel>, transport: TransportHandle, reference: Reference, kind: Kind, allowed: bool, writable: bool, on_changed: Callback<()>) -> Element {
    let mut open = use_signal(|| false);
    let mut query = use_signal(String::new);
    let mut reads = use_signal(|| 0u32);
    let mut answer = use_signal(|| None::<Result<Rc<Value>, String>>);
    let mut pending = use_signal(|| None::<String>);
    // The menu's search box, to give the focus back to it: the page re-renders after a change and the
    // browser drops the focus of the row that was clicked, which would leave Escape with nothing to hear it.
    let mut search = use_signal(|| None::<Rc<MountedData>>);
    let enabled = allowed && writable;

    // Read fresh at each opening and after each change; nothing is held while the menu is closed.
    {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_resource(move || {
            let (transport, reference, wanted, _reads) = (transport.clone(), reference.clone(), open(), reads());
            async move {
                if !wanted {
                    return;
                }
                let owner = runtime::response_owner(&transport, state);
                let result = runtime::request_value_reported(transport.clone(), state, kind.read(), reference.payload(json!({}))).await;
                if owner.is_none() || owner != runtime::response_owner(&transport, state) || !*open.peek() {
                    return;
                }
                answer.set(Some(result.map(Rc::new)));
            }
        });
    }
    // Whenever the candidates are read again (after a change), the focus goes back into the menu.
    use_effect(move || {
        let _ = answer.read();
        if *open.peek() {
            if let Some(box_) = search.peek().clone() {
                spawn(async move { let _ = box_.set_focus(true).await; });
            }
        }
    });
    let toggle = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |row: Row| {
            if pending.peek().is_some() || !enabled {
                return;
            }
            pending.set(Some(row.name.clone()));
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let mut extra = row.request.clone();
                extra[match kind { Kind::Reviewers => "requested", Kind::Labels => "applied" }] = json!(!row.on);
                let method = match kind {
                    Kind::Reviewers => "pullRequests.requestReviewers",
                    Kind::Labels => "pullRequests.setLabels",
                };
                let result = runtime::request_value_reported(transport.clone(), state, method, reference.payload(extra)).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match result {
                        Ok(_) => {
                            let (title, _) = changed(kind, &row, false);
                            if !title.is_empty() {
                                notice::push(state, Level::Success, title, "");
                            }
                            reads += 1;
                            on_changed.call(());
                        }
                        Err(message) => {
                            let (title, hint) = changed(kind, &row, true);
                            notice::push(state, Level::Error, title, pr::readable_failure(&message, hint));
                        }
                    }
                }
                pending.set(None);
                if *open.peek() {
                    if let Some(box_) = search.peek().clone() {
                        let _ = box_.set_focus(true).await;
                    }
                }
            });
        })
    };

    let typed = query.read().to_lowercase();
    let listing = answer.read().clone();
    rsx! {
        div { class: "pr-picker", onkeydown: move |event| { if event.key() == Key::Escape && open() { event.stop_propagation(); open.set(false); } },
            button { r#type: "button", "aria-label": "{kind.button()}", "aria-haspopup": "menu", "aria-expanded": "{open()}", title: if enabled { "" } else if !allowed { kind.reason() } else { "This connection cannot change source control." }, disabled: !enabled,
                onclick: move |_| { let next = !open(); if next { answer.set(None); query.set(String::new()); } open.set(next); }, "{kind.button()}" }
            if open() {
                // A click anywhere else closes the menu, as the original's popover does.
                div { class: "branch-picker-backdrop", "aria-hidden": "true", "aria-label": "Close {kind.button()} menu", onclick: move |_| open.set(false) }
                div { class: "pr-picker-menu", role: "menu", "aria-label": "{kind.button()}",
                    // Focus starts inside the menu, so Escape reaches it.
                    input { r#type: "search", "aria-label": "{kind.search()}", placeholder: "{kind.search()}", value: "{query}", oninput: move |event| query.set(event.value()),
                        onmounted: move |event| { let element = event.data(); search.set(Some(element.clone())); spawn(async move { let _ = element.set_focus(true).await; }); } }
                    match listing {
                        None => rsx! { p { class: "muted", role: "status", "Loading\u{2026}" } },
                        Some(Err(message)) => rsx! { p { class: "diff-error", role: "alert", "{kind.unreadable()} {message}" } },
                        Some(Ok(value)) => {
                            let all = rows(kind, &value);
                            let shown: Vec<Row> = all.iter().filter(|row| typed.is_empty() || row.name.to_lowercase().contains(&typed) || row.detail.as_deref().is_some_and(|detail| detail.to_lowercase().contains(&typed))).cloned().collect();
                            rsx! {
                                if shown.is_empty() { p { class: "muted", if typed.is_empty() { "{kind.empty()}" } else { "{kind.no_match()}" } } }
                                for row in shown {
                                    button { key: "{row.name}", r#type: "button", role: "menuitemcheckbox", "aria-checked": "{row.on}", "aria-label": "{row.name}", "aria-disabled": "{pending.read().is_some()}",
                                        onclick: { let row = row.clone(); move |_| toggle.call(row.clone()) },
                                        span { "{row.name}" }
                                        if let Some(detail) = row.detail.clone() { span { class: "muted", " \u{b7} {detail}" } }
                                        if row.on { span { "aria-label": if kind == Kind::Reviewers { "Already asked" } else { "Applied" }, " \u{2713}" } }
                                    }
                                }
                                if value["truncated"] == true { p { class: "muted", "{kind.truncated()}" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_become_rows_that_say_what_a_click_will_send() {
        let reviewers = json!({"candidates":[{"login":"bob","name":"Bob","id":"bob","kind":"user","isRequested":true},{"login":"carol","name":"carol","id":"carol","kind":"user","isRequested":false}]});
        let rows = rows(Kind::Reviewers, &reviewers);
        assert_eq!((rows[0].name.as_str(), rows[0].detail.as_deref(), rows[0].on), ("bob", Some("Bob"), true));
        assert_eq!(rows[1].detail, None, "a name that is the login says nothing more");
        assert_eq!(rows[0].request, json!({"reviewers":[{"id":"bob","kind":"user"}]}));
        let labels = rows_of_labels();
        assert_eq!((labels[0].name.as_str(), labels[0].detail.as_deref(), labels[0].on), ("bug", Some("Broken"), true));
        assert_eq!(labels[1].request, json!({"labels":["docs"]}));
        assert_eq!(changed(Kind::Reviewers, &rows[0], false).0, "Review request to bob taken back");
        assert_eq!(changed(Kind::Reviewers, &rows[1], true).0, "Could not ask carol for a review");
        assert_eq!(changed(Kind::Labels, &labels[0], true).0, "Could not take bug off");
        assert_eq!(changed(Kind::Labels, &labels[1], true).0, "Could not put docs on");
    }
    fn rows_of_labels() -> Vec<Row> {
        rows(Kind::Labels, &json!({"candidates":[{"name":"bug","description":"Broken","isApplied":true},{"name":"docs","description":null,"isApplied":false}]}))
    }
}
