//! Messages about something that finished somewhere the user is no longer looking: a project
//! made without its first commit, a thread without a project that could not start. They stay
//! until dismissed, and live beside the app-wide error banner rather than in it.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Notice {
    pub id: u64,
    pub level: Level,
    pub title: String,
    pub detail: String,
    /// What the notice offers to do next, when it offers anything.
    pub follow: Option<Follow>,
    /// A second thing it offers (the failure of a model started beside others offers both to open its thread and to allow a retry).
    pub also: Option<Follow>,
}
/// A call to action on a notice.
#[derive(Debug, Clone, PartialEq)]
pub enum Follow {
    /// Opens a page of the host (a pull request) in a new tab.
    Open { label: String, url: String },
    /// Runs a git action again, the way its menu entry would: the control that owns the
    /// workspace asks what the original asks first (the default branch).
    Git { label: String, cwd: String, action: t3_client::git_actions::Action },
    /// Opens a thread of a connected environment (the window moves to it first).
    Thread { label: String, environment: t3_contracts::EnvironmentId, thread_id: String },
    /// Asks whether to send again a message to a model whose first request may already have started a thread
    /// (`multi_model.rs`); the notice stays until the question is answered yes.
    AllowRetry { label: String, key: String, thread_id: String },
    /// Puts back the prompt of a message to several models that failed after the person had moved on
    /// (`multi_model.rs`); the notice stays until the prompt is back.
    RestorePrompt { label: String, token: u64 },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}
impl Level {
    fn class(self) -> &'static str {
        match self {
            Self::Info => "notice",
            Self::Success => "notice success",
            Self::Warning => "notice warning",
            Self::Error => "notice failure",
        }
    }
}

pub(crate) fn push(state: Store<UiModel>, level: Level, title: impl Into<String>, detail: impl Into<String>) {
    push_with(state, level, title, detail, None);
}

pub(crate) fn push_with(state: Store<UiModel>, level: Level, title: impl Into<String>, detail: impl Into<String>, follow: Option<Follow>) -> u64 {
    push_with_also(state, level, title, detail, follow, None)
}

/// `push_with` for a notice that offers two things.
pub(crate) fn push_with_also(state: Store<UiModel>, level: Level, title: impl Into<String>, detail: impl Into<String>, follow: Option<Follow>, also: Option<Follow>) -> u64 {
    let mut notices = state.notices();
    let mut list = notices.write();
    let id = list.last().map_or(1, |last| last.id + 1);
    // The newest few are enough; older ones have been seen.
    if list.len() >= 4 {
        list.remove(0);
    }
    list.push(Notice { id, level, title: title.into(), detail: detail.into(), follow, also });
    id
}

/// Puts a notice's outcome in its own place (a "Preparing..." line becomes its result), or as a new notice when it has been dismissed.
pub(crate) fn replace(state: Store<UiModel>, id: u64, level: Level, title: impl Into<String>, detail: impl Into<String>) {
    let (title, detail) = (title.into(), detail.into());
    let found = {
        let mut notices = state.notices();
        let mut list = notices.write();
        match list.iter_mut().find(|notice| notice.id == id) {
            Some(notice) => {
                (notice.level, notice.title, notice.detail, notice.follow, notice.also) = (level, title.clone(), detail.clone(), None, None);
                true
            }
            None => false,
        }
    };
    if !found {
        push(state, level, title, detail);
    }
}
pub(crate) fn dismiss(state: Store<UiModel>, id: u64) {
    state.notices().write().retain(|notice| notice.id != id);
}

#[component]
pub(crate) fn Host(state: Store<UiModel>, transport: crate::runtime::TransportHandle) -> Element {
    let notices = state.notices().read().clone();
    if notices.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "notices", role: "region", "aria-label": "Notices",
            crate::multi_model::RetryAsk { state }
            for notice in notices {
                div { key: "{notice.id}", class: notice.level.class(), role: if notice.level == Level::Error { "alert" } else { "status" },
                    div { class: "notice-text", strong { "{notice.title}" } if !notice.detail.is_empty() { crate::error_text::ErrorText { text: notice.detail.clone() } } }
                    for follow in [notice.follow.clone(), notice.also.clone()].into_iter().flatten() {
                        match follow {
                            Follow::Open { label, url } if url.starts_with("https://") || url.starts_with("http://") => rsx! {
                                a { class: "notice-follow", href: "{url}", target: "_blank", rel: "noopener noreferrer", onclick: { let id = notice.id; move |_| state.notices().write().retain(|notice| notice.id != id) }, "{label}" }
                            },
                            Follow::Git { label, cwd, action } => rsx! {
                                button { class: "notice-follow", r#type: "button", onclick: { let id = notice.id; move |_| {
                                    state.notices().write().retain(|notice| notice.id != id);
                                    crate::git_control::ask(state, &cwd, crate::git_control::Ask::Start(crate::git_control::Spec::of(action)));
                                } }, "{label}" }
                            },
                            Follow::Thread { label, environment, thread_id } => rsx! {
                                button { class: "notice-follow", r#type: "button", "aria-label": "{label}", onclick: { let (id, transport) = (notice.id, transport.clone()); move |_| {
                                    state.notices().write().retain(|notice| notice.id != id);
                                    crate::runtime::select_thread_in(&transport, state, &environment, thread_id.clone());
                                } }, "{label}" }
                            },
                            Follow::AllowRetry { label, key, thread_id } => rsx! {
                                button { class: "notice-follow", r#type: "button", "aria-label": "{label}", onclick: { let id = notice.id; move |_| crate::multi_model::ask_retry(state, id, key.clone(), thread_id.clone()) }, "{label}" }
                            },
                            Follow::RestorePrompt { label, token } => rsx! {
                                button { class: "notice-follow", r#type: "button", "aria-label": "{label}", onclick: { let id = notice.id; move |_| crate::multi_model::restore_prompt(state, id, token) }, "{label}" }
                            },
                            _ => rsx! {},
                        }
                    }
                    button { "aria-label": "Dismiss notice", onclick: { let id = notice.id; move |_| state.notices().write().retain(|notice| notice.id != id) }, "\u{d7}" }
                }
            }
        }
    }
}
