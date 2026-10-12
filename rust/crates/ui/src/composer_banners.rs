//! The notices that dock above the message box (`ComposerBanner.tsx`, `ComposerBannerStack.tsx`): one line each, with an icon,
//! a title, an optional second line, the actions that answer it and a way to dismiss it. Work in progress (the tasks badge, a goal,
//! background work) leads, then the urgent ones (an update running, an error or a warning), then the rest, as `t3_client::banner_stack` orders
//! them. The first is attached to the composer; the others wait behind it, shown as a slim cap that says "Show other notices", and come
//! out above it while the pointer is over them or the cap is clicked, and go back on leaving them or on Escape (the focus returning to the
//! cap). The stack draws nothing when it holds nothing, and nothing in it animates.
//!
//! Each notice is a stack item: `Item` registers it with the stack it is in, gives it its place and the front or behind attribute that the
//! stylesheet reads, and takes it out again when it goes. `Slot` says where a source of notices stands among the stack's sources, which is
//! the order the original passes its items in (a tie in priority keeps it).
//! TODO(original dismiss transition): the original lets a dismissed notice slide away for 220 ms before removing it; here it goes at once.
use dioxus::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use t3_client::banner_stack::{self as logic, Entry, Priority};

static NEXT: AtomicU64 = AtomicU64::new(1);
fn next() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// What the items of one stack share.
#[derive(Clone, Copy)]
struct Shared {
    /// The notices the stack holds, each with its variant (the cap takes the colour of the first one behind the front).
    entries: Signal<Vec<(Entry, &'static str)>>,
    expanded: Signal<bool>,
    /// The pointer is over a notice behind the front, or over the cap.
    hover: Signal<bool>,
    /// Focus goes to the cap on its next mount (Escape closed the group).
    focus_cap: Signal<bool>,
}
#[derive(Clone, Copy)]
struct SlotIndex(u32);

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
    let priority = logic::priority(None, urgent, variant.name());
    rsx! {
        Item { priority, variant,
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
}

/// The region the banners dock in. Its sources are `Slot`s, in the order the original passes its items in.
#[component]
pub(crate) fn Stack(children: Element) -> Element {
    let entries = use_signal(Vec::<(Entry, &'static str)>::new);
    let mut expanded = use_signal(|| false);
    let hover = use_signal(|| false);
    let mut focus_cap = use_signal(|| false);
    use_context_provider(|| Shared { entries, expanded, hover, focus_cap });
    let order = logic::sorted(&entries.read().iter().map(|(entry, _)| *entry).collect::<Vec<_>>());
    let count = order.len();
    // The cap takes the colour of the first notice behind the front.
    let cap_variant = order.get(1).and_then(|second| entries.read().iter().find(|(entry, _)| entry.id == second.id).map(|(_, variant)| *variant)).unwrap_or("default");
    // With nothing behind the front there is nothing to open.
    use_effect(move || {
        if entries.read().len() < 2 && *expanded.peek() {
            expanded.set(false);
        }
    });
    rsx! {
        div { class: "composer-banner-stack", role: "group", "aria-label": "Composer notices", "data-expanded": "{expanded}", "data-count": "{count}",
            onkeydown: move |event| {
                if event.key() == Key::Escape && *expanded.peek() {
                    event.prevent_default();
                    event.stop_propagation();
                    focus_cap.set(true);
                    expanded.set(false);
                }
            },
            {children}
            if count > 1 && !expanded() {
                button { class: "composer-banner-peek", r#type: "button", "aria-label": "Show other notices", "aria-expanded": "{expanded}", "data-variant": cap_variant, style: "order:999",
                    onmounted: move |event| {
                        if *focus_cap.peek() {
                            focus_cap.set(false);
                            let element = event.data();
                            spawn(async move { let _ = element.set_focus(true).await; });
                        }
                    },
                    onmouseenter: { let mut hover = hover; move |_| { hover.set(true); expanded.set(true); } },
                    onclick: move |_| {
                        expanded.set(true);
                        // The notices behind the cap take the keyboard, as the original moves it to the first control among them.
                        spawn(async move {
                            let _ = document::eval("requestAnimationFrame(() => document.querySelector('.composer-banner-stack .stack-item[data-stack-role=\"stacked\"] :is(button, a[href], input, [tabindex=\"0\"]):not(:disabled)')?.focus({ preventScroll: true }));").await;
                        });
                    },
                }
            }
        }
    }
}

/// A source of notices and where it stands among the stack's sources (lower comes first when priorities tie). Draws only what it holds.
#[component]
pub(crate) fn Slot(index: u32, children: Element) -> Element {
    use_context_provider(|| SlotIndex(index));
    rsx! { {children} }
}

/// One notice in the stack: wraps what the notice draws and says whether it is the front. A notice outside any stack is drawn as it is.
#[component]
pub(crate) fn Item(priority: Priority, #[props(default)] variant: Variant, children: Element) -> Element {
    let shared = use_hook(try_consume_context::<Shared>);
    let Some(shared) = shared else { return rsx! { {children} } };
    let slot = use_hook(|| try_consume_context::<SlotIndex>().map_or(0, |slot| slot.0));
    let (id, seq) = use_hook(|| (next(), next()));
    let name = variant.name();
    let mut entries = shared.entries;
    // Registered with its kind, and told again when the kind changes; a write that changes nothing is not made.
    use_effect(use_reactive((&priority, &name), move |(priority, name)| {
        let entry = (Entry { id, slot, seq, priority }, name);
        let known = entries.peek().iter().find(|(entry, _)| entry.id == id).copied();
        if known != Some(entry) {
            let mut list = entries.write();
            match list.iter_mut().find(|(entry, _)| entry.id == id) {
                Some(found) => *found = entry,
                None => list.push(entry),
            }
        }
    }));
    use_drop(move || entries.write().retain(|(entry, _)| entry.id != id));
    // Until it is registered a notice is shown, and in front.
    let (index, front) = logic::position(&entries.read().iter().map(|(entry, _)| *entry).collect::<Vec<_>>(), id).unwrap_or((0, true));
    // The notices behind the front are drawn above it in their order, the front last.
    let order = if front { 1000 } else { index as i32 };
    let mut expanded = shared.expanded;
    let mut hover = shared.hover;
    rsx! {
        div { class: "stack-item", "data-stack-role": if front { "front" } else { "stacked" }, style: "order:{order}",
            role: if front { None } else { Some("group") }, "aria-label": if front { None } else { Some("Other notices") },
            onmouseenter: move |_| { if !front { hover.set(true); expanded.set(true); } },
            // Moving from one notice to the next leaves one and enters the other in the same turn, so the group closes only if no
            // notice has been entered by the next.
            onmouseleave: move |_| {
                if !front {
                    hover.set(false);
                    spawn(async move {
                        crate::runtime::delay(0).await;
                        if !*hover.peek() { expanded.set(false); }
                    });
                }
            },
            // A click on the front puts the others back, as the original does on a press in the attached notice.
            onmousedown: move |_| { if front { expanded.set(false); } },
            {children}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::diff_tests::pump;
    use crate::runtime::keys_tests::key_on;
    use crate::runtime::transport_tests::{all_controls, click_control, control, control_attribute};

    fn banner(variant: Variant, urgent: bool, title: &str) -> Element {
        rsx! { Banner { variant, urgent, icon: "i".to_owned(), title: title.to_owned(), actions: rsx! {} } }
    }

    /// Three notices from three sources, given in the order the stack is told them.
    #[component]
    fn Three() -> Element {
        rsx! { Stack {
            Slot { index: 0, {banner(Variant::Default, false, "Quiet notice")} }
            Slot { index: 1, {banner(Variant::Warning, false, "Careful")} }
            Slot { index: 2, {banner(Variant::Info, true, "Updating")} }
        } }
    }
    #[component]
    fn One() -> Element {
        rsx! { Stack { Slot { index: 0, {banner(Variant::Default, false, "Only notice")} } } }
    }

    fn expanded(dom: &VirtualDom) -> Option<String> {
        control_attribute(dom, "Composer notices", "data-expanded").and_then(|(_, value)| value)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_notices_behind_the_front_wait_under_a_cap_that_opens_on_a_click_and_closes_on_escape() {
        let mut dom = VirtualDom::new(Three);
        dom.rebuild_in_place();
        pump(&mut dom).await;
        // The warning and the update lead the quiet notice; of those two the source given first is the front. Two wait behind it.
        assert_eq!(all_controls(&dom, "Other notices").len(), 2);
        assert_eq!(control_attribute(&dom, "Composer notices", "data-count").and_then(|(_, value)| value).as_deref(), Some("3"));
        assert_eq!(expanded(&dom).as_deref(), Some("false"));
        // The cap is the colour of the first notice behind the front: the update (info), not the quiet one.
        assert_eq!(control_attribute(&dom, "Show other notices", "data-variant").and_then(|(_, value)| value).as_deref(), Some("info"));
        assert_eq!(control_attribute(&dom, "Show other notices", "aria-expanded").and_then(|(_, value)| value).as_deref(), Some("false"));

        click_control(&mut dom, "Show other notices");
        pump(&mut dom).await;
        assert_eq!(expanded(&dom).as_deref(), Some("true"));
        assert!(control(&dom, "Show other notices").is_none(), "the cap gives way to what it covered");
        // Escape puts them back, and the cap is there again for the keyboard.
        key_on(&mut dom, "Composer notices", Key::Escape);
        pump(&mut dom).await;
        assert_eq!(expanded(&dom).as_deref(), Some("false"));
        assert!(control(&dom, "Show other notices").is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_lone_notice_has_no_cap_and_nothing_behind_it() {
        let mut dom = VirtualDom::new(One);
        dom.rebuild_in_place();
        pump(&mut dom).await;
        assert!(control(&dom, "Show other notices").is_none() && all_controls(&dom, "Other notices").is_empty());
        assert_eq!(control_attribute(&dom, "Composer notices", "data-count").and_then(|(_, value)| value).as_deref(), Some("1"));
    }
}
