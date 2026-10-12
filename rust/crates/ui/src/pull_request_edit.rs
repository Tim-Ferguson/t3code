//! Reactions on a pull request and rewriting its words in place: the title, the description and
//! your own remarks. Port of `PullRequestReactions.tsx`, `PullRequestEditButton.tsx` and the editing
//! of `PullRequestSummaryTab.tsx` / `PullRequestDetailPanel.tsx`.
//!
//! One edit is open at a time, and a save, like a press on a reaction, is a request that disables
//! its control until the host has answered. A failed save keeps the words in the editor. Escape
//! leaves an editor or a picker without leaving the panel.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use t3_client::{
    pull_request::{self as pr, Reference},
    pull_request_edit as logic,
};

/// What is being rewritten.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Edit {
    Title(String),
    Body(String),
    /// A remark, by the host's id for it.
    Comment(String, String),
}
/// What a save sends.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Save {
    Title(String),
    Body(String),
    Comment { id: String, kind: String, body: String },
}

/// The panel's editing, shared with the parts of it that draw an editor.
#[derive(Clone, Copy)]
pub(crate) struct Editing {
    pub(crate) edit: Signal<Option<Edit>>,
    pub(crate) saving: Signal<bool>,
    pub(crate) save: Callback<Save>,
}
pub(crate) fn editing() -> Option<Editing> {
    try_consume_context::<Editing>()
}

/// Provides the editing of one pull request to what the panel draws. `changed` reads the pull request again.
pub(crate) fn provide(state: Store<UiModel>, transport: TransportHandle, reference: Reference, writable: Memo<bool>, changed: Callback<()>) -> Editing {
    let edit = use_signal(|| None::<Edit>);
    let saving = use_signal(|| false);
    let save = use_callback(move |what: Save| {
        let mut saving = saving;
        let mut edit = edit;
        if *saving.peek() || !*writable.peek() {
            return;
        }
        let (title, hint, method, input) = match &what {
            Save::Title(title) => (logic::TITLE_FAILED, logic::TITLE_REFUSED, pr::UPDATE, json!({"title":title})),
            Save::Body(body) => (logic::BODY_FAILED, "The host refused the new description.", pr::UPDATE, json!({"body":body})),
            Save::Comment { id, kind, body } => (logic::COMMENT_FAILED, "The host refused the new words.", pr::UPDATE_COMMENT, json!({"commentId":id,"kind":kind,"body":body})),
        };
        saving.set(true);
        let (transport, reference) = (transport.clone(), reference.clone());
        spawn(async move {
            let release = Release(saving);
            let owner = runtime::response_owner(&transport, state);
            let answer = crate::pull_request_routing::request(transport.clone(), state, method, reference.payload(input)).await;
            if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                return;
            }
            match answer {
                Ok(_) => {
                    edit.set(None);
                    changed.call(());
                }
                // The editor stays open with the words still in it: retyping what somebody has just
                // rewritten is the one thing a failed save must not cost them.
                Err(message) => notice::push(state, Level::Error, title, pr::readable_failure(&message, hint)),
            }
            drop(release);
        });
    });
    use_context_provider(|| Editing { edit, saving, save })
}

/// Lets the saving flag go when the request ends, answered or abandoned.
struct Release(Signal<bool>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Ok(mut flag) = self.0.try_write() {
            *flag = false;
        }
    }
}

/// A pencil that opens an editor. It stays in the page for touch screens and keyboards.
#[component]
pub(crate) fn Pencil(label: String, disabled: bool, onclick: EventHandler<()>) -> Element {
    rsx! { button { class: "pr-edit-button", r#type: "button", "aria-label": "{label}", title: "{label}", disabled, onclick: move |_| onclick.call(()), "\u{270e}" } }
}

/// Words being rewritten: Escape leaves, Ctrl or Command with Enter saves.
#[component]
pub(crate) fn Editor(label: String, value: String, placeholder: String, saving: bool, on_save: EventHandler<String>, on_cancel: EventHandler<()>) -> Element {
    let mut draft = use_signal(|| value.clone());
    rsx! { div { class: "pr-editor",
        onkeydown: move |event| {
            if event.key() == Key::Escape {
                event.stop_propagation();
                if !saving { on_cancel.call(()); }
            } else if event.key() == Key::Enter && (event.modifiers().ctrl() || event.modifiers().meta()) {
                event.prevent_default();
                on_save.call(draft.peek().clone());
            }
        },
        textarea { "aria-label": "{label}", placeholder: "{placeholder}", value: "{draft}", disabled: saving,
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            oninput: move |event| draft.set(event.value()) }
        div { class: "pr-review-actions",
            button { r#type: "button", "aria-label": "Cancel editing", disabled: saving, onclick: move |_| on_cancel.call(()), "Cancel" }
            button { class: "primary", r#type: "button", "aria-label": "Save {label}", disabled: saving, onclick: move |_| on_save.call(draft.peek().clone()), if saving { "Saving..." } else { "Save" } }
        }
    } }
}

/// The title, with its pencil for who may rewrite it, or the one-line editor.
#[component]
pub(crate) fn TitleLine(title: String, can_edit: bool, writable: bool) -> Element {
    let Some(editing) = editing() else { return rsx! { h2 { class: "pr-title", "{title}" } } };
    let draft = match editing.edit.read().clone() {
        Some(Edit::Title(text)) => Some(text),
        _ => None,
    };
    let mut edit = editing.edit;
    let saving = *editing.saving.read();
    match draft {
        None => rsx! { div { class: "pr-title-line",
            h2 { class: "pr-title", "{title}" }
            if can_edit { Pencil { label: "Edit title", disabled: !writable || saving, onclick: { let title = title.clone(); move |_| edit.set(Some(Edit::Title(title.clone()))) } } }
        } },
        Some(text) => {
            let current = title.clone();
            let submit = move |text: String| {
                let mut edit = edit;
                let next = text.trim().to_owned();
                if next.is_empty() || next == current {
                    edit.set(None);
                } else {
                    editing.save.call(Save::Title(next));
                }
            };
            rsx! { div { class: "pr-editor pr-title-editor",
                onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); edit.set(None); } },
                input { "aria-label": "Pull request title", r#type: "text", value: "{text}", disabled: saving,
                    onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                    oninput: move |event| edit.set(Some(Edit::Title(event.value()))),
                    onkeydown: { let submit = submit.clone(); let text = text.clone(); move |event| { if event.key() == Key::Enter { event.prevent_default(); submit(text.clone()); } } } }
                div { class: "pr-review-actions",
                    button { r#type: "button", "aria-label": "Cancel title edit", disabled: saving, onclick: move |_| edit.set(None), "Cancel" }
                    button { class: "primary", r#type: "button", "aria-label": "Save title", disabled: !writable || saving || text.trim().is_empty(), onclick: { let text = text.clone(); move |_| submit(text.clone()) }, if saving { "Saving..." } else { "Save" } }
                }
            } }
        }
    }
}

/// The description's editor in place of its words, or the pencil that opens it.
#[component]
pub(crate) fn DescriptionEdit(body: String, can_edit: bool, writable: bool) -> Element {
    let Some(editing) = editing() else { return rsx! {} };
    let mut edit = editing.edit;
    let open = matches!(&*editing.edit.read(), Some(Edit::Body(_)));
    let saving = *editing.saving.read();
    if open {
        return rsx! { Editor { label: "Edit description", value: body, placeholder: "Describe this pull request", saving, on_save: move |text: String| editing.save.call(Save::Body(text)), on_cancel: move |_| edit.set(None) } };
    }
    rsx! { if can_edit { Pencil { label: "Edit description", disabled: !writable || saving, onclick: { let body = body.clone(); move |_| edit.set(Some(Edit::Body(body.clone()))) } } } }
}

/// One remark's rewriting: the pencil, or the editor in place of its words. `children` are its words.
#[component]
pub(crate) fn CommentEdit(id: String, kind: String, body: String, can_edit: bool, writable: bool, children: Element) -> Element {
    let Some(editing) = editing() else { return rsx! { {children} } };
    let mut edit = editing.edit;
    let open = matches!(&*editing.edit.read(), Some(Edit::Comment(open, _)) if *open == id);
    let saving = *editing.saving.read();
    if open {
        let (id, kind) = (id.clone(), kind.clone());
        return rsx! { Editor { label: "Edit comment", value: body, placeholder: "", saving,
            on_save: move |text: String| editing.save.call(Save::Comment { id: id.clone(), kind: kind.clone(), body: text }), on_cancel: move |_| edit.set(None) } };
    }
    rsx! { div { class: "pr-comment-text", {children}
        if can_edit { Pencil { label: "Edit comment", disabled: !writable || saving, onclick: { let (id, body) = (id.clone(), body.clone()); move |_| edit.set(Some(Edit::Comment(id.clone(), body.clone()))) } } }
    } }
}

/// The reactions on the description or on a remark, and the picker for adding one. A press shows at
/// once and is put back if the host refuses it; its pill waits for the answer before it can be pressed again.
#[component]
pub(crate) fn Reactions(state: Store<UiModel>, transport: TransportHandle, reference: Reference, reactions: Vec<Value>, subject: Option<String>, can_react: bool, writable: bool, on_changed: Callback<()>) -> Element {
    let mut pending = use_signal(|| (String::new(), BTreeMap::<String, bool>::new()));
    let mut in_flight = use_signal(BTreeSet::<String>::new);
    let mut open = use_signal(|| false);
    let signature = logic::signature(&reactions);
    let values = if pending.read().0 == signature { pending.read().1.clone() } else { BTreeMap::new() };
    let shown = logic::apply_pending(&reactions, &values);
    let toggle = {
        let (transport, reference, subject, signature) = (transport.clone(), reference.clone(), subject.clone(), signature.clone());
        use_callback(move |(content, reacted): (String, bool)| {
            if !can_react || !writable || in_flight.peek().contains(&content) {
                return;
            }
            in_flight.write().insert(content.clone());
            let mut next = if pending.peek().0 == signature { pending.peek().1.clone() } else { BTreeMap::new() };
            next.insert(content.clone(), reacted);
            pending.set((signature.clone(), next));
            let (transport, reference, subject, signature) = (transport.clone(), reference.clone(), subject.clone(), signature.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let mut input = json!({"content":content,"reacted":reacted});
                if let Some(subject) = &subject {
                    input["subjectId"] = json!(subject);
                }
                let answer = crate::pull_request_routing::request(transport.clone(), state, pr::SET_REACTION, reference.payload(input)).await;
                in_flight.write().remove(&content);
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                match answer {
                    Ok(_) => on_changed.call(()),
                    Err(_) => {
                        let mut current = pending.peek().clone();
                        if current.0 == signature {
                            current.1.remove(&content);
                            pending.set(current);
                        }
                        notice::push(state, Level::Error, logic::REACTION_FAILED, "");
                    }
                }
            });
        })
    };
    if shown.is_empty() && !can_react {
        return rsx! {};
    }
    rsx! { div { class: "pr-reactions", onkeydown: move |event| { if *open.peek() && event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
        for reaction in shown.iter().cloned() {
            { let content = reaction["content"].as_str().unwrap_or_default().to_owned(); let reacted = reaction["viewerHasReacted"] == true; let busy = in_flight.read().contains(&content); let count = reaction["count"].as_u64().unwrap_or(0);
              rsx! { button { key: "{content}", class: "pr-reaction", r#type: "button", "aria-pressed": "{reacted}", "aria-label": "{logic::reaction_name(&content)}, {count}", title: "{logic::reaction_tooltip(&reaction)}",
                disabled: !can_react || !writable || busy, onclick: { let content = content.clone(); move |_| toggle.call((content.clone(), !reacted)) },
                span { "aria-hidden": "true", "{logic::reaction_emoji(&content)}" } " {count}" } } }
        }
        if can_react {
            button { class: "pr-reaction", r#type: "button", "aria-label": "Add a reaction", "aria-expanded": "{open}", disabled: !writable, onclick: move |_| { let now = *open.peek(); open.set(!now) }, "\u{263a}+" }
            if *open.read() {
                div { class: "pr-reaction-picker", role: "group", "aria-label": "Reactions",
                    for content in logic::REACTION_ORDER {
                        { let reacted = shown.iter().any(|reaction| reaction["content"] == content && reaction["viewerHasReacted"] == true);
                          rsx! { button { key: "{content}", class: "pr-reaction", r#type: "button", "aria-pressed": "{reacted}", "aria-label": "{logic::reaction_name(content)}", disabled: !writable || in_flight.read().contains(content),
                            onclick: move |_| { open.set(false); toggle.call((content.to_owned(), !reacted)) }, span { "aria-hidden": "true", "{logic::reaction_emoji(content)}" } } } }
                    }
                }
            }
        }
    } }
}
