//! The notices that dock above the message box (`ComposerBanner.tsx`, `ComposerBannerStack.tsx`): one line each, with an icon,
//! a title, an optional second line, the actions that answer it and a way to dismiss it. Urgent ones (an update running)
//! come before passive ones. The stack draws nothing when it holds nothing, and a banner is never an animation.
use dioxus::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Variant {
    #[default]
    Default,
    Error,
    Info,
    Success,
    Warning,
}
impl Variant {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Error => "error",
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
        }
    }
}

#[component]
pub(crate) fn Banner(
    variant: Variant,
    urgent: bool,
    icon: String,
    title: String,
    /// The line after the title, clipped at its end so a long detail never squeezes the title.
    #[props(default)]
    detail: Option<String>,
    #[props(default)] tooltip: Option<String>,
    #[props(default = "status")] role: &'static str,
    #[props(default)] description: Option<String>,
    actions: Element,
    #[props(default)] dismiss_label: Option<String>,
    #[props(default)] on_dismiss: Option<EventHandler<()>>,
) -> Element {
    let hover = tooltip.clone().or_else(|| detail.as_ref().map(|detail| format!("{title}: {detail}"))).unwrap_or_default();
    let label = detail.as_ref().map(|detail| format!("{title}: {detail}")).unwrap_or_else(|| title.clone());
    rsx! {
        div { class: "composer-banner", "data-variant": variant.name(), "data-priority": if urgent { "urgent" } else { "notice" }, role: "{role}",
            span { class: "composer-banner-icon", "aria-hidden": "true", "{icon}" }
            div { class: "composer-banner-body",
                span { class: "composer-banner-title", title: "{hover}", "aria-label": "{label}",
                    "{title}"
                    if let Some(detail) = detail { span { class: "composer-banner-separator", "aria-hidden": "true", " \u{b7} " } span { class: "muted", "{detail}" } }
                }
                if let Some(description) = description { span { class: "composer-banner-description muted", "{description}" } }
            }
            div { class: "composer-banner-actions", {actions} }
            if let (Some(label), Some(on_dismiss)) = (dismiss_label, on_dismiss) {
                button { r#type: "button", class: "composer-banner-dismiss", "aria-label": "{label}", title: "{label}", onclick: move |_| on_dismiss.call(()), "\u{d7}" }
            }
        }
    }
}

/// The region the banners dock in.
#[component]
pub(crate) fn Stack(children: Element) -> Element {
    rsx! { div { class: "composer-banner-stack", {children} } }
}
