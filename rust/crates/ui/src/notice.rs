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
}
/// A call to action on a notice.
#[derive(Debug, Clone, PartialEq)]
pub enum Follow {
    /// Opens a page of the host (a pull request) in a new tab.
    Open { label: String, url: String },
    /// Runs a git action again, the way its menu entry would: the control that owns the
    /// workspace asks what the original asks first (the default branch).
    Git { label: String, cwd: String, action: t3_client::git_actions::Action },
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

pub(crate) fn push_with(state: Store<UiModel>, level: Level, title: impl Into<String>, detail: impl Into<String>, follow: Option<Follow>) {
    let mut notices = state.notices();
    let mut list = notices.write();
    let id = list.last().map_or(1, |last| last.id + 1);
    // The newest few are enough; older ones have been seen.
    if list.len() >= 4 {
        list.remove(0);
    }
    list.push(Notice { id, level, title: title.into(), detail: detail.into(), follow });
}

#[component]
pub(crate) fn Host(state: Store<UiModel>) -> Element {
    let notices = state.notices().read().clone();
    if notices.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "notices", role: "region", "aria-label": "Notices",
            for notice in notices {
                div { key: "{notice.id}", class: notice.level.class(), role: if notice.level == Level::Error { "alert" } else { "status" },
                    div { class: "notice-text", strong { "{notice.title}" } if !notice.detail.is_empty() { span { "{notice.detail}" } } }
                    match notice.follow.clone() {
                        Some(Follow::Open { label, url }) if url.starts_with("https://") || url.starts_with("http://") => rsx! {
                            a { class: "notice-follow", href: "{url}", target: "_blank", rel: "noopener noreferrer", onclick: { let id = notice.id; move |_| state.notices().write().retain(|notice| notice.id != id) }, "{label}" }
                        },
                        Some(Follow::Git { label, cwd, action }) => rsx! {
                            button { class: "notice-follow", r#type: "button", onclick: { let id = notice.id; move |_| {
                                state.notices().write().retain(|notice| notice.id != id);
                                let next = state.peek().git_follow.as_ref().map_or(1, |follow| follow.id + 1);
                                state.git_follow().set(Some(crate::git_control::FollowRequest { id: next, cwd: cwd.clone(), action }));
                            } }, "{label}" }
                        },
                        _ => rsx! {},
                    }
                    button { "aria-label": "Dismiss notice", onclick: { let id = notice.id; move |_| state.notices().write().retain(|notice| notice.id != id) }, "\u{d7}" }
                }
            }
        }
    }
}
