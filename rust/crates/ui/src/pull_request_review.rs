//! Reviewing a pull request: the composer (a comment, or a review), the review form (verdict and summary, submitted once with
//! the line comments the Code tab collected), those pending line comments and the box one is written in, and the review
//! threads with their replies, their resolution and the rest of a long thread. Port of `PullRequestComposer.tsx`,
//! `PullRequestCommentForm.tsx`, `PullRequestReviewForm.tsx` and `PullRequestReviewAnnotation.tsx`.
//!
//! Threads are drawn under their line of the diff (those the diff no longer shows, under their file). Every write is sent once
//! and its control is enabled again by the server's answer; the panel reads again when one succeeds.
use crate::{
    markdown::Markdown,
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::rc::Rc;
use t3_client::{pull_request::{self as pr, Reference}, pull_request_line_comments as pr_comments};

/// When something happened, as the original words it ("3h ago"); the full time is the tooltip.
pub(crate) fn when(at: &str) -> String {
    t3_client::prompt_stash::relative_label(at, chrono::Utc::now())
}

/// The verdicts, in the order the form lists them: wire name, label, what the toast says.
pub(crate) const VERDICTS: [(&str, &str, &str); 3] = [("comment", "Comment", "Review submitted"), ("approve", "Approve", "Pull request approved"), ("request-changes", "Request changes", "Changes requested")];

/// The review being written for this pull request, read without subscribing to every keystroke of anything.
pub(crate) fn review_key(reference: &Reference) -> String {
    reference.key()
}

/// The pending remarks a review holds for a pull request.
pub(crate) fn pending_of(state: Store<UiModel>, key: &str) -> Vec<t3_client::pull_request_line_comments::Pending> {
    state.pr_reviews().read().get(key).map(|draft| draft.comments.clone()).unwrap_or_default()
}

/// The review half of the composer: the summary and the verdict that sends it with the line comments it holds. The summary is
/// the field's own text until it is left or sent, so typing it repaints nothing else.
#[component]
pub(crate) fn ReviewForm(state: Store<UiModel>, transport: TransportHandle, reference: Reference, verdicts: Vec<String>, summary_required_to_request_changes: bool, writable: bool, why: Option<String>, on_done: Callback<()>) -> Element {
    let key = review_key(&reference);
    let mut body = use_signal({
        let key = key.clone();
        move || state.pr_reviews().peek().get(&key).map(|draft| draft.summary.clone()).unwrap_or_default()
    });
    let mut chosen = use_signal(|| None::<String>);
    let sending = use_signal(|| false);
    // What was typed outlives this form: leaving the field or closing the composer keeps it for the next time.
    let keep = {
        let key = key.clone();
        use_callback(move |_: ()| {
            let text = body.peek().clone();
            let mut reviews = state.pr_reviews();
            let mut reviews = reviews.write();
            if reviews.get(&key).is_some_and(|draft| draft.summary == text) || (text.is_empty() && !reviews.contains_key(&key)) {
                return;
            }
            reviews.entry(key.clone()).or_default().summary = text;
        })
    };
    let offered: Vec<(&str, &str, &str)> = VERDICTS.iter().copied().filter(|(wire, ..)| verdicts.iter().any(|verdict| verdict == wire)).collect();
    if offered.is_empty() {
        return rsx! {};
    }
    let pending = pending_of(state, &key);
    let verdict = chosen.read().clone().filter(|chosen| offered.iter().any(|(wire, ..)| wire == chosen)).unwrap_or_else(|| offered[0].0.to_owned());
    // Rebuilt on every draw: it reads the verdict and the grants as they are now, not as they were at the first draw.
    let submit = {
        let (transport, reference, verdict, key) = (transport.clone(), reference.clone(), verdict.clone(), key.clone());
        Rc::new(move || {
            let (mut body, mut sending) = (body, sending);
            let text = body.peek().clone();
            let comments = pending_of(state, &key);
            if *sending.peek() || !writable || !pr_comments::can_submit(&verdict, &text, comments.len(), summary_required_to_request_changes) {
                return;
            }
            sending.set(true);
            let (transport, reference, verdict, key) = (transport.clone(), reference.clone(), verdict.clone(), key.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let wire: Vec<Value> = comments.iter().map(pr_comments::Pending::wire).collect();
                let answer = runtime::request_value_reported(transport.clone(), state, "pullRequests.submitReview", reference.payload(json!({"verdict":verdict,"body":text,"comments":wire}))).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(_) => {
                            // Remarks added while the host was answering, and a summary revised meanwhile, are the next review's.
                            let ids: Vec<u64> = comments.iter().map(|comment| comment.id).collect();
                            if let Some(draft) = state.pr_reviews().write().get_mut(&key) {
                                draft.sent(&ids, &text);
                            }
                            if *body.peek() == text {
                                body.set(String::new());
                            }
                            let sent = VERDICTS.iter().find(|(wire, ..)| *wire == verdict).map_or("Review submitted", |(.., sent)| *sent);
                            notice::push(state, Level::Success, sent, "");
                            on_done.call(());
                        }
                        // The draft is kept: whatever went wrong, retyping the review is not the answer.
                        Err(message) => notice::push(state, Level::Error, "The review could not be submitted", pr::readable_failure(&message, "The host refused it. Check that you may review this pull request.")),
                    }
                }
                sending.set(false);
            });
        })
    };
    let ready = pr_comments::can_submit(&verdict, &body.read(), pending.len(), summary_required_to_request_changes);
    let placeholder = if summary_required_to_request_changes && verdicts.iter().any(|verdict| verdict == "request-changes") { "Summarize your review (required to request changes)" } else { "Summarize your review (optional)" };
    rsx! {
        form { class: "pr-review-form", "aria-label": "Review", onsubmit: move |event| event.prevent_default(),
            textarea { rows: "3", "aria-label": "Review summary", placeholder, value: "{body}", disabled: !writable || *sending.read(), oninput: move |event| body.set(event.value()), onchange: move |_| keep.call(()), onblur: move |_| keep.call(()) }
            div { class: "pr-review-actions",
                select { "aria-label": "Review verdict", value: "{verdict}", disabled: !writable || *sending.read(), onchange: move |event| chosen.set(Some(event.value())),
                    for (wire, label, _) in offered.iter() { option { key: "{wire}", value: "{wire}", selected: *wire == verdict, "{label}" } }
                }
                button { class: "primary", r#type: "button", "aria-label": "Submit review", title: why.clone().unwrap_or_default(), disabled: !writable || *sending.read() || !ready, onclick: { let submit = submit.clone(); move |_| submit() },
                    if *sending.read() { "Submitting\u{2026}" } else { "Submit review" } }
            }
        }
    }
}

/// The one control over a pull request: a remark on the change, or the review that carries the line comments of the Code tab.
/// Opening picks the mode with work waiting in it, and either mode's draft survives the toggle and the closing.
#[component]
pub(crate) fn Composer(state: Store<UiModel>, transport: TransportHandle, reference: Reference, detail: Rc<Value>, writable: bool, busy: Option<String>, why: Option<String>, on_commented: Callback<()>, on_action: Callback<(String, Option<String>)>, on_review_sent: Callback<()>) -> Element {
    let key = review_key(&reference);
    let mut open = use_signal(|| false);
    let mut mode_review = use_signal(|| false);
    let mut comment = use_signal(String::new);
    let posting = use_signal(|| None::<&'static str>);
    let can_comment = detail["capabilities"]["comment"] == true && detail["viewerPermissions"]["comment"] == true;
    let verdicts: Vec<String> = {
        let may: Vec<&str> = detail["viewerPermissions"]["verdicts"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        detail["capabilities"]["review"]["verdicts"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|verdict| may.contains(verdict)).map(str::to_owned).collect()
    };
    if !can_comment && verdicts.is_empty() {
        return rsx! {};
    }
    let pending = pending_of(state, &key);
    let started = state.pr_reviews().read().get(&key).is_some_and(|draft| draft.started());
    let review_mode = if can_comment { !verdicts.is_empty() && mode_review() } else { true };
    let follow_up = match detail["state"].as_str() {
        Some("open") if pr::can_perform(&detail, "close") => Some("close"),
        Some("closed") if pr::can_perform(&detail, "reopen") => Some("reopen"),
        _ => None,
    };
    // Rebuilt on every draw, so the grants and the write in flight are read as they are now.
    let send: Rc<dyn Fn(&'static str)> = {
        let (transport, reference, busy) = (transport.clone(), reference.clone(), busy.clone());
        Rc::new(move |action: &'static str| {
            let (mut comment, mut posting, mut open) = (comment, posting, open);
            let text = comment.peek().trim().to_owned();
            if text.is_empty() || posting.peek().is_some() || busy.is_some() || !writable {
                return;
            }
            posting.set(Some(action));
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, pr::COMMENT, reference.payload(json!({"body":text}))).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(_) => {
                            if comment.peek().trim() == text {
                                comment.set(String::new());
                            }
                            open.set(false);
                            on_commented.call(());
                            // Closing or reopening with the comment: the remark is already at the host.
                            if action != "comment" {
                                on_action.call((action.to_owned(), None));
                            }
                        }
                        Err(message) => notice::push(state, Level::Error, "Could not post the comment", pr::readable_failure(&message, "The host refused it. Check that you may comment on this pull request.")),
                    }
                }
                posting.set(None);
            });
        })
    };
    let label = if !pending.is_empty() {
        format!("Review pull request, {} {} pending", pending.len(), if pending.len() == 1 { "comment" } else { "comments" })
    } else if started || !can_comment {
        "Review pull request".to_owned()
    } else {
        "Comment on pull request".to_owned()
    };
    let disabled_post = comment.read().trim().is_empty() || posting.read().is_some() || busy.is_some() || !writable;
    rsx! {
        div { class: "pr-composer",
            button { class: "pr-composer-toggle", r#type: "button", "aria-label": "{label}", "aria-expanded": "{open()}",
                onclick: move |_| { let now = *open.peek(); if !now { mode_review.set(started); } open.set(!now); },
                "\u{1f4ac}" if !pending.is_empty() { span { class: "pr-pending-count", "aria-hidden": "true", "{pending.len()}" } } }
            // Both forms stay mounted while the composer is closed, so a draft and a request in flight survive closing it.
            div { class: "pr-composer-popup", role: "dialog", "aria-label": "Pull request composer", hidden: !open(),
                onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
                div { class: "pr-composer-head",
                    if can_comment && !verdicts.is_empty() {
                        div { class: "pr-composer-modes", role: "group", "aria-label": "Composer mode",
                            button { r#type: "button", "aria-label": "Comment mode", "aria-pressed": "{!review_mode}", onclick: move |_| mode_review.set(false), "Comment" }
                            button { r#type: "button", "aria-label": "Review mode", "aria-pressed": "{review_mode}", onclick: move |_| mode_review.set(true), if pending.is_empty() { "Review" } else { "Review ({pending.len()})" } }
                        }
                    } else { strong { if review_mode { "Review pull request" } else { "Comment on pull request" } } }
                    if review_mode && !pending.is_empty() {
                        button { r#type: "button", "aria-label": "Discard pending line comments", title: "Discard pending line comments", disabled: !writable,
                            onclick: { let key = key.clone(); move |_| { if let Some(draft) = state.pr_reviews().write().get_mut(&key) { draft.comments.clear(); } } }, "\u{1f5d1}" }
                    }
                    button { r#type: "button", "aria-label": "Close composer", onclick: move |_| open.set(false), "\u{2715}" }
                }
                if !verdicts.is_empty() {
                    div { hidden: !review_mode,
                        ReviewForm { state, transport: transport.clone(), reference: reference.clone(), verdicts: verdicts.clone(), summary_required_to_request_changes: detail["provider"] == "forgejo", writable, why: why.clone(),
                            on_done: move |_| { open.set(false); on_review_sent.call(()); } }
                    }
                }
                if can_comment {
                    div { hidden: review_mode,
                        form { class: "pr-comment-form", onsubmit: { let send = send.clone(); move |event| { event.prevent_default(); send("comment"); } },
                            textarea { "aria-label": "Comment on this pull request", rows: "3", placeholder: "Leave a comment", value: "{comment}", disabled: !writable || posting.read().is_some(),
                                oninput: move |event| comment.set(event.value()),
                                onkeydown: { let send = send.clone(); move |event| { if event.key() == Key::Enter && (event.modifiers().meta() || event.modifiers().ctrl()) && !event.modifiers().shift() { event.prevent_default(); event.stop_propagation(); send("comment"); } } } }
                            div { class: "pr-composer-actions",
                                if let Some(action) = follow_up {
                                    button { class: if action == "close" { "destructive" } else { "" }, r#type: "button", disabled: disabled_post, "aria-label": if action == "close" { "Close with comment" } else { "Reopen with comment" },
                                        onclick: { let send = send.clone(); move |_| send(action) },
                                        if posting.read().as_deref() == Some(action) { if action == "close" { "Closing..." } else { "Reopening..." } } else if action == "close" { "Close with comment" } else { "Reopen with comment" } }
                                }
                                button { class: "primary", r#type: "button", "aria-label": "Comment", disabled: disabled_post, onclick: { let send = send.clone(); move |_| send("comment") }, if posting.read().as_deref() == Some("comment") { "Posting..." } else { "Comment" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A line comment the review holds and has not sent, under the line it was made on.
#[component]
pub(crate) fn PendingCard(state: Store<UiModel>, key_of_review: String, comment: pr_comments::Pending) -> Element {
    let id = comment.id;
    rsx! { div { class: "pr-pending-comment", "data-pending": "true",
        div { class: "pr-comment-head", strong { "Pending" } span { class: "muted", "{comment.label()}" }
            button { r#type: "button", "aria-label": "Discard pending comment on {comment.label()}", title: "Discard this pending comment",
                onclick: move |_| { if let Some(draft) = state.pr_reviews().write().get_mut(&key_of_review) { draft.remove(id); } }, "\u{2715}" } }
        Markdown { text: comment.body.clone() }
    } }
}

/// The box a line comment is written in, until it is added to the review or dropped.
#[component]
pub(crate) fn LineDraft(label: String, on_add: EventHandler<String>, on_cancel: EventHandler<()>) -> Element {
    let mut text = use_signal(String::new);
    rsx! { form { class: "pr-line-draft", "aria-label": "Comment on {label}", onsubmit: move |event| event.prevent_default(),
        textarea { "aria-label": "Line comment on {label}", rows: "3", placeholder: "Leave a comment", value: "{text}",
            onmounted: move |event| { drop(event.set_focus(true)); },
            oninput: move |event| text.set(event.value()),
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); on_cancel.call(()); } } }
        div { class: "pr-composer-actions",
            button { r#type: "button", "aria-label": "Cancel line comment", onclick: move |_| on_cancel.call(()), "Cancel" }
            button { class: "primary", r#type: "button", "aria-label": "Add to review", disabled: text.read().trim().is_empty(), onclick: move |_| { let body = text.peek().trim().to_owned(); if !body.is_empty() { on_add.call(body); } }, "Add to review" }
        }
    } }
}

/// The comments a thread shows: what the activity read carried, then the pages read since.
fn comments_of(thread: &Value, extra: Option<&(Option<String>, Vec<Value>, Option<String>)>, base: &Option<String>) -> (Vec<Value>, Option<String>) {
    let mut comments = thread["comments"].as_array().cloned().unwrap_or_default();
    match extra {
        Some((from, more, next)) if from == base => {
            comments.extend(more.iter().cloned());
            (comments, next.clone())
        }
        _ => (comments, base.clone()),
    }
}

#[component]
pub(crate) fn Threads(state: Store<UiModel>, transport: TransportHandle, reference: Reference, threads: Vec<Value>, can_reply: bool, can_resolve: bool, writable: bool, why: Option<String>, on_changed: Callback<()>) -> Element {
    let mut drafts = use_signal(BTreeMap::<String, String>::new);
    let mut acting = use_signal(|| None::<String>);
    // Pages read after the first: the cursor they started from, the comments, and the next cursor.
    let mut pages = use_signal(BTreeMap::<String, (Option<String>, Vec<Value>, Option<String>)>::new);
    let run = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |(method, extra, failed, hint, id): (&'static str, Value, &'static str, &'static str, String)| {
            if acting.peek().is_some() || !writable {
                return;
            }
            acting.set(Some(id.clone()));
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, method, reference.payload(extra)).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(_) => {
                            if method == "pullRequests.replyToThread" {
                                drafts.write().remove(&id);
                            }
                            on_changed.call(());
                        }
                        Err(message) => notice::push(state, Level::Error, failed, pr::readable_failure(&message, hint)),
                    }
                }
                acting.set(None);
            });
        })
    };
    let more = {
        let (transport, reference) = (transport.clone(), reference.clone());
        use_callback(move |(thread_id, base, cursor): (String, Option<String>, String)| {
            if acting.peek().is_some() {
                return;
            }
            acting.set(Some(thread_id.clone()));
            let (transport, reference) = (transport.clone(), reference.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "pullRequests.threadComments", reference.payload(json!({"threadId":thread_id,"cursor":cursor}))).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(page) => {
                            let mut write = pages.write();
                            let entry = write.entry(thread_id.clone()).or_insert((base.clone(), vec![], None));
                            if entry.0 != base {
                                *entry = (base.clone(), vec![], None);
                            }
                            entry.1.extend(page["comments"].as_array().cloned().unwrap_or_default());
                            entry.2 = page["nextCursor"].as_str().map(str::to_owned);
                        }
                        Err(message) => notice::push(state, Level::Error, "Could not load more comments", pr::readable_failure(&message, "The host refused it.")),
                    }
                }
                acting.set(None);
            });
        })
    };
    rsx! {
        div { class: "pr-threads",
            for thread in threads.iter() {
                {
                    let id = thread["id"].as_str().unwrap_or_default().to_owned();
                    let path = thread["path"].as_str().unwrap_or_default().to_owned();
                    let line = thread["line"].as_u64();
                    let resolved = thread["isResolved"] == true;
                    let outdated = thread["isOutdated"] == true;
                    let base = thread["nextCommentsCursor"].as_str().map(str::to_owned);
                    let (comments, next) = comments_of(thread, pages.read().get(&id), &base);
                    let draft = drafts.read().get(&id).cloned().unwrap_or_default();
                    let busy = acting.read().is_some();
                    let (reply_id, resolve_id, more_id) = (id.clone(), id.clone(), id.clone());
                    rsx! {
                        section { key: "{id}", class: "pr-thread", "aria-label": "Review thread on {path}", "data-resolved": "{resolved}",
                            header { class: "pr-thread-head",
                                strong { "{path}" } if let Some(line) = line { span { class: "muted", ":{line}" } }
                                if resolved { span { class: "pr-state", "Resolved" } }
                                if outdated { span { class: "pr-state", "Outdated" } }
                            }
                            ol { class: "pr-comments",
                                for comment in comments.iter() {
                                    li { key: "{comment[\"id\"].as_str().unwrap_or_default()}", class: "pr-comment",
                                        div { class: "pr-comment-head", strong { "{comment[\"author\"][\"login\"].as_str().unwrap_or(\"ghost\")}" } span { class: "muted", title: "{comment[\"createdAt\"].as_str().unwrap_or_default()}", "{when(comment[\"createdAt\"].as_str().unwrap_or_default())}" } }
                                        if !comment["body"].as_str().unwrap_or_default().trim().is_empty() { Markdown { text: comment["body"].as_str().unwrap_or_default().to_owned() } }
                                    }
                                }
                            }
                            if let Some(cursor) = next {
                                button { r#type: "button", "aria-label": "Show more comments in the thread on {path}", disabled: busy, onclick: { let base = base.clone(); move |_| more.call((more_id.clone(), base.clone(), cursor.clone())) }, "Show more comments" }
                            }
                            if can_resolve {
                                button { r#type: "button", "aria-label": if resolved { "Unresolve thread on {path}" } else { "Resolve thread on {path}" }, title: why.clone().unwrap_or_default(), disabled: !writable || busy,
                                    onclick: move |_| run.call(("pullRequests.setThreadResolution", json!({"threadId":resolve_id,"resolved":!resolved}), if resolved { "Could not unresolve this conversation" } else { "Could not resolve this conversation" }, "The host refused it. Check that you have write access, or that you opened this pull request.", resolve_id.clone())),
                                    if resolved { "Unresolve conversation" } else { "Resolve conversation" } }
                            }
                            if can_reply {
                                textarea { "aria-label": "Reply to thread on {path}", rows: "2", placeholder: "Reply", value: "{draft}", disabled: !writable || busy,
                                    oninput: { let id = id.clone(); move |event| { drafts.write().insert(id.clone(), event.value()); } } }
                                button { r#type: "button", "aria-label": "Reply in the thread on {path}", title: why.clone().unwrap_or_default(), disabled: !writable || busy || draft.trim().is_empty(),
                                    onclick: { let draft = draft.clone(); move |_| run.call(("pullRequests.replyToThread", json!({"threadId":reply_id,"body":draft.trim()}), "Could not post your reply", "The host refused it. Check that you may comment on this pull request.", reply_id.clone())) }, "Reply" }
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
    fn a_thread_shows_its_first_comments_and_then_the_pages_read_from_the_same_cursor() {
        let thread = json!({"comments":[{"id":"a"}]});
        let base = Some("c1".to_owned());
        let extra = (base.clone(), vec![json!({"id":"b"})], Some("c2".to_owned()));
        let (comments, next) = comments_of(&thread, Some(&extra), &base);
        assert_eq!((comments.len(), next.as_deref()), (2, Some("c2")));
        // A refresh that starts from another cursor drops pages read from the old one.
        let (comments, next) = comments_of(&thread, Some(&extra), &Some("c9".to_owned()));
        assert_eq!((comments.len(), next.as_deref()), (1, Some("c9")));
        assert_eq!(comments_of(&thread, None, &None), (vec![json!({"id":"a"})], None));
    }
}
