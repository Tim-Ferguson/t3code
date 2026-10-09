//! Reviewing a pull request: the review form (verdict and summary, submitted once) and the review
//! threads with their replies, their resolution and the rest of a long thread. Port of
//! `PullRequestReviewForm.tsx` and the thread part of `PullRequestReviewAnnotation.tsx`.
//!
//! Threads are drawn with the file they belong to, below that file's diff, and not at their
//! line; commenting on a line of the diff is not ported. Every write is sent once and its
//! control is enabled again by the server's answer; the panel reads again when one succeeds.
use crate::{
    markdown::Markdown,
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use t3_client::pull_request::{self as pr, Reference};

/// The verdicts, in the order the form lists them: wire name, label, what the toast says.
pub(crate) const VERDICTS: [(&str, &str, &str); 3] = [("comment", "Comment", "Review submitted"), ("approve", "Approve", "Pull request approved"), ("request-changes", "Request changes", "Changes requested")];

/// Whether a review can be sent: an approval may be empty; anything else needs a summary.
pub(crate) fn can_submit(verdict: &str, summary: &str) -> bool {
    verdict == "approve" || !summary.trim().is_empty()
}

#[component]
pub(crate) fn ReviewForm(state: Store<UiModel>, transport: TransportHandle, reference: Reference, verdicts: Vec<String>, writable: bool, why: Option<String>, on_done: Callback<()>) -> Element {
    let mut body = use_signal(String::new);
    let mut chosen = use_signal(|| None::<String>);
    let mut sending = use_signal(|| false);
    let offered: Vec<(&str, &str, &str)> = VERDICTS.iter().copied().filter(|(wire, ..)| verdicts.iter().any(|verdict| verdict == wire)).collect();
    if offered.is_empty() {
        return rsx! {};
    }
    let verdict = chosen.read().clone().filter(|chosen| offered.iter().any(|(wire, ..)| wire == chosen)).unwrap_or_else(|| offered[0].0.to_owned());
    let submit = {
        let (transport, reference, verdict) = (transport.clone(), reference.clone(), verdict.clone());
        use_callback(move |_: ()| {
            let text = body.peek().clone();
            if *sending.peek() || !writable || !can_submit(&verdict, &text) {
                return;
            }
            sending.set(true);
            let (transport, reference, verdict) = (transport.clone(), reference.clone(), verdict.clone());
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "pullRequests.submitReview", reference.payload(json!({"verdict":verdict,"body":text,"comments":[]}))).await;
                if owner.is_some() && owner == runtime::response_owner(&transport, state) {
                    match answer {
                        Ok(_) => {
                            // Text typed while the host was answering stays for the next review.
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
    let ready = can_submit(&verdict, &body.read());
    rsx! {
        form { class: "pr-review-form", "aria-label": "Review", onsubmit: move |event| event.prevent_default(),
            textarea { rows: "3", "aria-label": "Review summary", placeholder: "Summarize your review (optional)", value: "{body}", disabled: !writable || *sending.read(), oninput: move |event| body.set(event.value()) }
            div { class: "pr-review-actions",
                select { "aria-label": "Review verdict", value: "{verdict}", disabled: !writable || *sending.read(), onchange: move |event| chosen.set(Some(event.value())),
                    for (wire, label, _) in offered.iter() { option { key: "{wire}", value: "{wire}", selected: *wire == verdict, "{label}" } }
                }
                button { class: "primary", r#type: "button", "aria-label": "Submit review", title: why.clone().unwrap_or_default(), disabled: !writable || *sending.read() || !ready, onclick: move |_| submit.call(()),
                    if *sending.read() { "Submitting\u{2026}" } else { "Submit review" } }
            }
        }
    }
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
pub(crate) fn Threads(state: Store<UiModel>, transport: TransportHandle, reference: Reference, threads: Vec<Value>, can_resolve: bool, writable: bool, why: Option<String>, on_changed: Callback<()>) -> Element {
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
                                        div { class: "pr-comment-head", strong { "{comment[\"author\"][\"login\"].as_str().unwrap_or(\"ghost\")}" } span { class: "muted", "{comment[\"createdAt\"].as_str().unwrap_or_default()}" } }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_approval_may_be_empty_and_anything_else_needs_words() {
        assert!(can_submit("approve", "") && can_submit("approve", "  "));
        assert!(!can_submit("comment", "  ") && !can_submit("request-changes", ""));
        assert!(can_submit("request-changes", "Please fix") && can_submit("comment", "x"));
    }

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
