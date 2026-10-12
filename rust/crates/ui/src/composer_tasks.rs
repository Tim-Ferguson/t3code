//! The tasks badge over the composer while a turn runs (`ComposerTasksBadge.tsx`): one line saying how far the plan has got ("Tasks",
//! the step in progress, `2/5`, a bar of segments), which opens the list of steps with how long each took. It sits first in the
//! banner stack, as the original's activity item does, and draws nothing when the latest run is over or has no plan.
//! The rules are `t3_client::composer_tasks`.
use crate::{
    composer_panels::latest_run_settled,
    runtime::{UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use t3_client::composer_tasks::{self as logic, Status};

/// The plan of the open thread's running turn as a badge, folded until the reader opens it. It folds again for another thread.
#[component]
pub(crate) fn Tasks(state: Store<UiModel>, thread_id: String) -> Element {
    let mut expanded = use_signal(|| false);
    let plan = use_memo(use_reactive((&thread_id,), move |(thread_id,)| {
        let field = state.thread();
        let model = field.read();
        let projection = model.projection.as_ref().filter(|projection| projection["thread"]["id"] == thread_id.as_str())?;
        logic::active(projection, latest_run_settled(projection).is_some())
    }));
    let Some((progress, steps)) = plan.read().clone() else { return rsx! {} };
    let open = expanded();
    let label = logic::badge_label(open, &progress);
    let done = progress.completed_steps >= progress.total_steps;
    let segments = steps.len() > 1 && steps.len() <= logic::MAX_SEGMENTS;
    rsx! {
        div { class: "composer-tasks", "data-composer-tasks-drawer": if open { "true" } else { "false" },
            button { r#type: "button", class: "composer-tasks-badge", "aria-expanded": "{open}", "aria-label": "{label}", "data-composer-tasks-badge": "true",
                // The click must not take focus out of the message box.
                onmousedown: move |event| event.prevent_default(),
                onclick: move |_| expanded.set(!open),
                span { class: "composer-banner-icon", "aria-hidden": "true", "\u{2630}" }
                span { class: "muted", "Tasks" }
                span { class: "composer-tasks-current", "data-composer-task-current": "true", "{progress.step}" }
                span { class: if done { "composer-tasks-count success" } else { "composer-tasks-count" }, "data-composer-task-progress": "true", "{progress.completed_steps}/{progress.total_steps}" }
                if segments {
                    span { class: "composer-tasks-segments", "aria-hidden": "true",
                        for (index, step) in steps.iter().enumerate() {
                            span { key: "{index}", class: match step.status { Status::Completed => "segment completed", Status::InProgress => "segment running", Status::Pending => "segment" } }
                        }
                    }
                }
                span { class: "fold-chevron", "aria-hidden": "true", if open { "\u{25be}" } else { "\u{25b8}" } }
            }
            if open {
                ul { role: "list", class: "composer-tasks-list", "aria-label": "{logic::list_label(&progress)}", "data-composer-tasks-list": "true",
                    for (index, step) in steps.iter().enumerate() {
                        li { key: "{index}:{step.step}", class: match step.status { Status::Completed => "completed", Status::InProgress => "running", Status::Pending => "pending" },
                            span { class: "step-status", "aria-hidden": "true", match step.status { Status::Completed => "\u{2713}", Status::InProgress => "\u{25c9}", Status::Pending => "\u{25cb}" } }
                            span { class: "sr-only", "{step.status.label()}: " }
                            span { class: "composer-tasks-step", "{step.step}" }
                            span { class: "composer-tasks-duration muted", "data-composer-task-duration": "true", {logic::duration_label(step).unwrap_or_default()} }
                        }
                    }
                }
            }
        }
    }
}
