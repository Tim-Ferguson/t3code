//! Context menus: one `show(items, position)` that answers with the id chosen, as the original's `contextMenu.show` does. The desktop
//! app draws the menu natively (`desktop_shell::native_menu`, the original's `showContextMenu` over IPC); everywhere else, and
//! where the system has no such menu to offer, it is drawn in the page (`contextMenuFallback.ts`) by the `ContextMenuHost` the
//! application mounts once. The items and what the system keeps of them are `t3_client::context_menu`.
use dioxus::prelude::*;
use futures_channel::oneshot;
use std::{cell::RefCell, rc::Rc, sync::atomic::{AtomicU64, Ordering}};
use t3_client::context_menu::Item;

/// The menu open in the page, if any. Only one at a time: a menu shown while another is open ends the other with no choice.
#[derive(Clone)]
struct Open {
    id: u64,
    /// What the menu is for, as a screen reader names it.
    label: String,
    items: Vec<Item>,
    at: Option<(f64, f64)>,
    reply: Rc<RefCell<Option<oneshot::Sender<Option<String>>>>>,
}
impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Open {
    fn answer(&self, choice: Option<String>) {
        if let Some(reply) = self.reply.borrow_mut().take() {
            let _ = reply.send(choice);
        }
    }
}

static OPEN: GlobalSignal<Option<Open>> = Signal::global(|| None);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Ends the open menu with no choice (`dismissContextMenu`): for the state that made it pointless, such as a selection that cleared.
#[allow(dead_code)]
pub(crate) fn close() {
    let open = OPEN.peek().clone();
    if let Some(open) = open {
        open.answer(None);
        *OPEN.write() = None;
    }
}

/// Removes the menu its future opened when that future is dropped before an answer: the caller left while the menu was up.
struct Pending(u64);
impl Drop for Pending {
    fn drop(&mut self) {
        if dioxus::dioxus_core::Runtime::try_current().is_some() && OPEN.peek().as_ref().is_some_and(|open| open.id == self.0) {
            *OPEN.write() = None;
        }
    }
}

/// Shows a menu at a position in the page's pixels (the pointer's place when there is none) and answers with the id of the leaf
/// chosen, or none when it was dismissed or had nothing to show.
pub(crate) async fn show(label: &str, items: Vec<Item>, at: Option<(f64, f64)>) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    #[cfg(feature = "desktop")]
    if let Some(answer) = crate::desktop_shell::native_menu(&items, at) {
        return answer.await;
    }
    close();
    let (reply, answer) = oneshot::channel();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let guard = Pending(id);
    *OPEN.write() = Some(Open { id, label: label.to_owned(), items, at, reply: Rc::new(RefCell::new(Some(reply))) });
    // The element that had focus gets it back when the menu closes without taking it (`previouslyFocusedElement`).
    let _ = document::eval("window.__t3MenuReturn = document.activeElement;");
    let choice = answer.await.ok().flatten();
    drop(guard);
    let _ = document::eval("{ const e = window.__t3MenuReturn; window.__t3MenuReturn = null; const a = document.activeElement; if (e && e.isConnected && (!a || a === document.body || a.closest('[role=menu]'))) e.focus({ preventScroll: true }); }");
    choice
}

/// The icon a name stands for (the original's table of Lucide paths), as the elements inside a 24-pixel box.
fn icon_paths(name: &str) -> Option<&'static str> {
    Some(match name {
        "archive" => r#"<rect width="20" height="5" x="2" y="3" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/><path d="M10 12h4"/>"#,
        "check" => r#"<path d="M20 6 9 17l-5-5"/>"#,
        "timer" => r#"<line x1="10" x2="14" y1="2" y2="2"/><line x1="12" x2="15" y1="14" y2="11"/><circle cx="12" cy="14" r="8"/>"#,
        "chevron-right" => r#"<path d="m9 19 7-7-7-7"/>"#,
        "circle-check" => r#"<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>"#,
        "clock" => r#"<path d="M12 6v6l4 2"/><circle cx="12" cy="12" r="10"/>"#,
        "pencil" => r#"<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/><path d="m15 5 4 4"/>"#,
        "copy" => r#"<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#,
        "folder" => r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#,
        "git-branch" => r#"<line x1="6" x2="6" y1="3" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/>"#,
        "hash" => r#"<line x1="4" x2="20" y1="9" y2="9"/><line x1="4" x2="20" y1="15" y2="15"/><line x1="10" x2="8" y1="3" y2="21"/><line x1="16" x2="14" y1="3" y2="21"/>"#,
        "mail-open" => r#"<path d="M21.2 8.4c.5.38.8.97.8 1.6v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V10a2 2 0 0 1 .8-1.6l8-6a2 2 0 0 1 2.4 0l8 6Z"/><path d="m22 10-8.97 5.7a1.94 1.94 0 0 1-2.06 0L2 10"/>"#,
        "message-square-plus" => r#"<path d="M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"/><path d="M12 8v6"/><path d="M9 11h6"/>"#,
        "pin" => r#"<path d="M12 17v5"/><path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z"/>"#,
        "pin-off" => r#"<path d="M12 17v5"/><path d="M15 9.34V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H7.89"/><path d="m2 2 20 20"/><path d="M9 9v1.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h11"/>"#,
        "refresh-cw" => r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>"#,
        "settings" => r#"<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.38a2 2 0 0 0-.73-2.73l-.15-.09a2 2 0 0 1-1-1.74v-.51a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>"#,
        "folder-tree" => r#"<path d="M20 10a1 1 0 0 0 1-1V6a1 1 0 0 0-1-1h-2.5a1 1 0 0 1-.8-.4l-.9-1.2A1 1 0 0 0 15 3h-2a1 1 0 0 0-1 1v5a1 1 0 0 0 1 1Z"/><path d="M20 21a1 1 0 0 0 1-1v-3a1 1 0 0 0-1-1h-2.9a1 1 0 0 1-.88-.55l-.42-.85a1 1 0 0 0-.92-.6H13a1 1 0 0 0-1 1v5a1 1 0 0 0 1 1Z"/><path d="M3 5a2 2 0 0 0 2 2h3"/><path d="M3 3v13a2 2 0 0 0 2 2h3"/>"#,
        "trash" => r#"<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><line x1="10" x2="10" y1="11" y2="17"/><line x1="14" x2="14" y1="11" y2="17"/>"#,
        _ => return None,
    })
}

#[component]
fn Icon(name: String, tone: &'static str) -> Element {
    let Some(paths) = icon_paths(&name) else { return rsx! {} };
    rsx! { svg { class: "menu-icon {tone}", xmlns: "http://www.w3.org/2000/svg", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true", dangerous_inner_html: paths } }
}

/// The menu in the page, mounted once by the application. Draws nothing while no menu is open.
#[component]
pub(crate) fn ContextMenuHost() -> Element {
    let Some(open) = OPEN.read().clone() else { return rsx! {} };
    let dismiss = Callback::new({
        let open = open.clone();
        move |()| {
            open.answer(None);
            *OPEN.write() = None;
        }
    });
    let choose = {
        let open = open.clone();
        Callback::new(move |id: String| {
            open.answer(Some(id));
            *OPEN.write() = None;
        })
    };
    let (x, y) = open.at.unwrap_or((0.0, 0.0));
    rsx! { Fragment { key: "{open.id}",
        // Anywhere outside the menu ends it, a right-click included (which does not open the page's own menu under it).
        div { class: "menu-backdrop context-menu-backdrop", onclick: move |_| dismiss.call(()), oncontextmenu: move |event| { event.prevent_default(); dismiss.call(()); } }
        MenuList { label: open.label.clone(), items: open.items.clone(), level: 0usize, at: (x, y), anchor_left: x, choose, dismiss, back: None::<Callback<()>>, focus_first: false }
    } }
}

/// One menu of the stack: the top one, or the submenu an item with children opens beside it.
#[component]
fn MenuList(label: String, items: Vec<Item>, level: usize, at: (f64, f64), anchor_left: f64, choose: Callback<String>, dismiss: Callback<()>, back: Option<Callback<()>>, focus_first: bool) -> Element {
    // The submenu that is open: the item it belongs to, whether it takes the focus, and the place beside that item's row once known.
    let mut sub = use_signal(|| None::<(String, bool, Option<(f64, f64, f64)>)>);
    let mut triggers = use_signal(Vec::<(String, Rc<MountedData>)>::new);
    let open_sub = use_callback(move |(id, focus): (String, bool)| {
        let row = triggers.peek().iter().find(|(key, _)| *key == id).map(|(_, row)| row.clone());
        sub.set(Some((id.clone(), focus, None)));
        spawn(async move {
            if let Some(rect) = match row { Some(row) => row.get_client_rect().await.ok(), None => None } {
                if sub.peek().as_ref().is_some_and(|(open, ..)| *open == id) {
                    sub.set(Some((id, focus, Some((rect.max_x() + 2.0, rect.min_y() - 5.0, rect.min_x())))));
                }
            }
        });
    });
    let style = format!("position:fixed;left:{}px;top:{}px", at.0, at.1);
    let open_child = sub.read().clone();
    rsx! {
        div { class: "context-menu context-menu-list", role: "menu", "aria-label": "{label}", tabindex: "0", style: "{style}", "data-context-level": "{level}", "data-anchor-left": "{anchor_left}",
            onmounted: move |event| {
                // The menu is fitted into the window once it exists and has a size: kept off the edges, a submenu flipped to the left of its row.
                let _ = document::eval(&format!("{{ const m = document.querySelector('[data-context-level=\"{level}\"]'); if (m) {{ const r = m.getBoundingClientRect(); let left = r.left, top = r.top; if ({level} > 0 && r.right > innerWidth - 4) left = Number(m.dataset.anchorLeft) - r.width - 4; m.style.left = Math.max(4, Math.min(left, innerWidth - r.width - 4)) + 'px'; m.style.top = Math.max(4, Math.min(top, innerHeight - r.height - 4)) + 'px'; }} }}"));
                if focus_first {
                    let _ = document::eval(&format!("document.querySelector('[data-context-level=\"{level}\"] button.menu-item:not(:disabled)')?.focus()"));
                } else {
                    drop(event.data().set_focus(true));
                }
            },
            onkeydown: move |event| {
                match event.key() {
                    Key::Escape => { event.stop_propagation(); event.prevent_default(); dismiss.call(()); }
                    Key::ArrowDown | Key::ArrowUp | Key::Home | Key::End => {
                        event.prevent_default();
                        let step = match event.key() { Key::ArrowDown => "1", Key::ArrowUp => "-1", Key::Home => "'first'", _ => "'last'" };
                        let _ = document::eval(&format!("{{ const m = document.querySelector('[data-context-level=\"{level}\"]'); if (m) {{ const b = [...m.querySelectorAll(':scope > button.menu-item:not(:disabled)')]; if (b.length) {{ const s = {step}; const i = b.indexOf(document.activeElement); b[s === 'first' ? 0 : s === 'last' ? b.length - 1 : (i + s + b.length) % b.length].focus(); }} }} }}"));
                    }
                    Key::ArrowLeft if level > 0 => { event.prevent_default(); event.stop_propagation(); if let Some(back) = back { back.call(()); } }
                    _ => {}
                }
            },
            for (index, item) in items.iter().cloned().enumerate() {
                if item.separator_before && index > 0 { div { key: "sep-{index}", class: "menu-separator", role: "separator" } }
                if item.header {
                    div { key: "head-{index}", class: "menu-header", "{item.label}" }
                } else if let Some(children) = item.submenu().map(<[Item]>::to_vec) {
                    {
                        let id = item.id.clone();
                        let expanded = open_child.as_ref().is_some_and(|(open, ..)| *open == id);
                        rsx! {
                            button { key: "{id}", class: "menu-item submenu-trigger", role: "menuitem", r#type: "button", "aria-label": "{item.label}", "aria-haspopup": "menu", "aria-expanded": "{expanded}", disabled: item.disabled,
                                onmounted: { let id = id.clone(); move |event| { let mut list = triggers.write(); list.retain(|(key, _)| *key != id); list.push((id.clone(), event.data())); } },
                                onmouseenter: { let id = id.clone(); move |_| { if !expanded { open_sub.call((id.clone(), false)); } } },
                                onclick: { let id = id.clone(); move |event| { event.stop_propagation(); open_sub.call((id.clone(), true)); } },
                                onkeydown: { let id = id.clone(); move |event| { if event.key() == Key::ArrowRight { event.prevent_default(); event.stop_propagation(); open_sub.call((id.clone(), true)); } } },
                                if let Some(icon) = item.icon.clone() { Icon { name: icon, tone: "neutral" } }
                                span { class: "menu-label", "{item.label}" }
                                Icon { name: "chevron-right".to_owned(), tone: "chevron" }
                            }
                            if let Some((_, takes_focus, place)) = open_child.clone().filter(|(open, ..)| *open == id) {
                                MenuList { key: "{id}-menu", label: item.label.clone(), items: children, level: level + 1, at: place.map_or((0.0, 0.0), |(left, top, _)| (left, top)), anchor_left: place.map_or(0.0, |(_, _, anchor)| anchor), choose, dismiss,
                                    back: Callback::new({ let id = id.clone(); move |()| { sub.set(None); if let Some((_, row)) = triggers.peek().iter().find(|(key, _)| *key == id) { let row = row.clone(); spawn(async move { drop(row.set_focus(true).await); }); } } }),
                                    focus_first: takes_focus }
                            }
                        }
                    }
                } else {
                    {
                        let id = item.id.clone();
                        let destructive = item.looks_destructive();
                        let tone: &'static str = if destructive { "destructive" } else { "neutral" };
                        let item_role: &'static str = if item.checked.is_some() { "menuitemradio" } else { "menuitem" };
                        let item_class: &'static str = if destructive { "menu-item destructive" } else { "menu-item" };
                        let aria_checked: Option<&'static str> = item.checked.map(|checked| if checked { "true" } else { "false" });
                        rsx! {
                            button { key: "{id}-{index}", class: item_class, role: item_role, "aria-checked": aria_checked, r#type: "button", "aria-label": "{item.label}", disabled: item.disabled,
                                onmouseenter: move |_| { if sub.peek().is_some() { sub.set(None); } },
                                onclick: move |event| { event.stop_propagation(); choose.call(id.clone()); },
                                // Option rows use the icon's place for the check, so labels line up; the other option keeps the place empty.
                                if let Some(checked) = item.checked {
                                    if checked { Icon { name: "check".to_owned(), tone: "neutral" } } else { span { class: "menu-icon", "aria-hidden": "true" } }
                                } else if let Some(icon) = item.icon.clone() {
                                    Icon { name: icon, tone }
                                }
                                span { class: "menu-label", "{item.label}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
