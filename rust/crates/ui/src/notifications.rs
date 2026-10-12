//! Telling the person that a thread they are not looking at finished, failed or needs them: a toast while this
//! window has focus, a system notification and a badge while it does not, and a sound. Port of
//! `ThreadNotificationCoordinator.tsx`, `threadNotifications.ts` and `NotificationSettings.tsx`; what makes an
//! alert is `t3_client::thread_notifications`.
//!
//! The web build uses the browser's Notification API (asking for permission when the setting is chosen, as the
//! original does) and a badge drawn into the tab icon. The desktop build presents the notification through the
//! system and puts the count on the dock icon. Nothing runs while both settings are off.
use crate::{
    fleet,
    notice::{self, Follow, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_rows,
};
use base64::Engine;
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};
use t3_client::{
    connection::ConnectionStatus,
    thread_notifications::{self as logic, Alert, Kind, Tracker},
};
use t3_contracts::{EnvironmentId, NotificationMode};

const COMPLETION_SOUND: &[u8] = include_bytes!("../assets/notification-completion.mp3");
const INPUT_SOUND: &[u8] = include_bytes!("../assets/notification-input.mp3");

/// The page's half: the Notification API, audio that a gesture has unlocked, and the tab icon with a count.
const PAGE: &str = r#"
if (!window.__t3Notify) {
  const pending = new Map();
  let context = null;
  const buffers = new Map();
  let original = null, badge = null;
  const unlock = () => { context ??= new AudioContext(); context.resume().catch(() => {}); };
  window.__t3Notify = {
    watchGestures(on) {
      for (const name of ['pointerdown', 'keydown']) { document.removeEventListener(name, unlock); if (on) document.addEventListener(name, unlock); }
    },
    focused: () => document.visibilityState === 'visible' && document.hasFocus(),
    permission: () => (typeof Notification === 'undefined' ? 'unsupported' : Notification.permission),
    async request() {
      if (typeof Notification === 'undefined' || !window.isSecureContext) return 'unsupported';
      try { return await Notification.requestPermission(); } catch { return 'error'; }
    },
    async play(kind, base64) {
      if (!context || context.state !== 'running') return false;
      try {
        let buffer = buffers.get(kind);
        if (!buffer) {
          const bytes = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
          buffer = context.decodeAudioData(bytes.buffer);
          buffers.set(kind, buffer);
        }
        const source = context.createBufferSource();
        source.buffer = await buffer;
        source.connect(context.destination);
        source.start();
        return true;
      } catch { buffers.delete(kind); return false; }
    },
    show(tag, title, body) {
      try {
        pending.get(tag)?.close();
        const notification = new Notification(title, { body, tag, silent: true });
        pending.set(tag, notification);
        notification.addEventListener('click', () => { notification.close(); window.focus(); window.__t3NotifyClick?.(tag); });
        return true;
      } catch { return false; }
    },
    clear() { for (const notification of pending.values()) notification.close(); pending.clear(); },
    // A count drawn into the tab icon, where there is no dock to badge.
    badge(count) {
      if (count > 0) {
        const canvas = document.createElement('canvas');
        canvas.width = canvas.height = 64;
        const draw = canvas.getContext('2d');
        draw.fillStyle = '#e5484d'; draw.beginPath(); draw.arc(32, 32, 28, 0, Math.PI * 2); draw.fill();
        draw.fillStyle = 'white'; draw.font = `600 ${count > 9 ? 30 : 40}px sans-serif`; draw.textAlign = 'center'; draw.textBaseline = 'middle';
        draw.fillText(count > 9 ? '9+' : String(count), 32, 34);
        if (!badge) {
          original = document.querySelector('link[rel="icon"]');
          badge = document.createElement('link'); badge.rel = 'icon'; badge.type = 'image/png';
          original?.remove(); document.head.append(badge);
        }
        badge.href = canvas.toDataURL('image/png');
      } else if (badge) {
        badge.remove(); badge = null;
        if (original) document.head.append(original);
        original = null;
      }
    },
  };
}
"#;

fn eval_page(call: &str) -> document::Eval {
    document::eval(&format!("{PAGE}\n{call}"))
}

#[cfg(feature = "desktop")]
const DESKTOP: bool = true;
#[cfg(not(feature = "desktop"))]
const DESKTOP: bool = false;

/// The count on the dock icon (desktop) or in the tab icon (web).
fn show_badge(count: usize) {
    #[cfg(feature = "desktop")]
    crate::desktop_shell::set_badge(count);
    #[cfg(not(feature = "desktop"))]
    {
        let _ = eval_page(&format!("window.__t3Notify.badge({count});"));
    }
}

/// The "Thread notifications" row: choosing a way that needs the system's permission asks for it first, and a refusal is
/// explained where the row is.
#[component]
pub(crate) fn ModeRow(state: Store<UiModel>) -> Element {
    let mut message = use_signal(|| None::<&'static str>);
    let mut requesting = use_signal(|| false);
    let current = serde_json::to_value(state.client_settings().read().notification_mode).unwrap_or(Value::Null);
    let choose = EventHandler::new(move |value: Value| {
        let Ok(mode) = serde_json::from_value::<NotificationMode>(value.clone()) else { return };
        message.set(None);
        spawn(async move {
            if logic::has_sound(mode) {
                let _ = eval_page("window.__t3Notify.watchGestures(true); document.dispatchEvent(new Event('keydown'));").join::<()>().await;
            }
            if logic::has_desktop_notifications(mode) && !DESKTOP {
                requesting.set(true);
                let answer = eval_page("return await window.__t3Notify.request();").join::<String>().await.unwrap_or_else(|_| "error".into());
                requesting.set(false);
                match answer.as_str() {
                    "granted" => {}
                    "unsupported" => return message.set(Some(logic::UNSUPPORTED)),
                    "error" => return message.set(Some(logic::UNAVAILABLE)),
                    _ => return message.set(Some(logic::PERMISSION_DENIED)),
                }
            }
            settings_rows::patch(state, json!({ "notificationMode": value }));
        });
    });
    rsx! {
        { settings_rows::control(settings_rows::NOTIFICATION_MODE, &current, requesting(), choose) }
        if let Some(text) = message() { p { class: "muted", role: "status", "{text}" } }
    }
}

/// Watches every connected environment's threads, the parked ones' too, and announces what changed while nobody was looking
/// (`EnvironmentNotifications` of the original, one for each environment).
#[component]
pub(crate) fn Coordinator(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let trackers = use_hook(|| Rc::new(RefCell::new(BTreeMap::<EnvironmentId, Tracker>::new())));
    // Tags of system notifications not yet seen; their number is the badge.
    let pending = use_hook(|| Rc::new(RefCell::new(BTreeSet::<String>::new())));
    let mut clicked = use_signal(|| None::<String>);

    // A click on a system notification, and the window coming back: the notifications are seen.
    use_future({
        let pending = pending.clone();
        move || {
            let pending = pending.clone();
            async move {
                let mut page = eval_page("window.__t3NotifyClick = (tag) => dioxus.send({kind:'click', tag}); window.addEventListener('focus', () => dioxus.send({kind:'focus'})); await dioxus.recv();");
                while let Ok(message) = page.recv::<Value>().await {
                    match message["kind"].as_str() {
                        Some("click") => clicked.set(message["tag"].as_str().map(str::to_owned)),
                        Some("focus") => {
                            if !pending.borrow().is_empty() {
                                pending.borrow_mut().clear();
                                let _ = eval_page("window.__t3Notify.clear();");
                                show_badge(0);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    });
    use_effect({
        let transport = transport.clone();
        move || {
            let Some(tag) = clicked.read().clone() else { return };
            clicked.set(None);
            if let Some((environment, thread)) = fleet::split(state, &tag) {
                runtime::select_thread_in(&transport, state, &environment, thread);
            }
        }
    });

    // The settings: sound needs a gesture to have unlocked it; leaving a mode that shows notifications clears them.
    use_effect({
        let pending = pending.clone();
        move || {
            let mode = state.client_settings().read().notification_mode;
            let _ = eval_page(&format!("window.__t3Notify.watchGestures({});", logic::has_sound(mode)));
            if !pending.borrow().is_empty() {
                pending.borrow_mut().clear();
                let _ = eval_page("window.__t3Notify.clear();");
                show_badge(0);
            }
        }
    });

    use_effect({
        let (trackers, pending, transport) = (trackers.clone(), pending.clone(), transport.clone());
        move || {
            let settings = state.client_settings().read().clone();
            let (mode, in_app) = (settings.notification_mode, settings.in_app_notifications_enabled);
            if mode == NotificationMode::Off && !in_app {
                trackers.borrow_mut().clear();
                return;
            }
            let alerts: Vec<(EnvironmentId, Alert)> = fleet::read(state, |shells| {
                let mut trackers = trackers.borrow_mut();
                // An environment that is gone is forgotten; one that is not live starts from nothing when it is.
                trackers.retain(|environment, _| shells.iter().any(|shell| shell.environment == environment));
                let mut alerts = vec![];
                for shell in shells {
                    let tracker = trackers.entry(shell.environment.clone()).or_default();
                    if fleet::status(state, shell.environment) == ConnectionStatus::Connected {
                        alerts.extend(tracker.scan(&shell.snapshot.threads).into_iter().map(|alert| (shell.environment.clone(), alert)));
                    } else {
                        tracker.reset();
                    }
                }
                alerts
            });
            for (environment, alert) in alerts {
                present(state, &transport, &pending, &environment, mode, in_app, alert);
            }
        }
    });
    rsx! {}
}

/// Whether this window is the one the person is looking at. A test has no page to ask: it is.
async fn window_focused() -> bool {
    #[cfg(test)]
    return true;
    #[cfg(not(test))]
    eval_page("return window.__t3Notify.focused();").join::<bool>().await.unwrap_or(true)
}

fn present(state: Store<UiModel>, transport: &TransportHandle, pending: &Rc<RefCell<BTreeSet<String>>>, environment: &EnvironmentId, mode: NotificationMode, in_app: bool, alert: Alert) {
    let (pending, environment, transport) = (pending.clone(), environment.clone(), transport.clone());
    spawn(async move {
        let _ = &transport;
        if logic::has_sound(mode) {
            let (kind, bytes) = if alert.kind == Kind::Completion { ("completion", COMPLETION_SOUND) } else { ("input", INPUT_SOUND) };
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let _ = eval_page(&format!("return await window.__t3Notify.play('{kind}', '{encoded}');")).join::<bool>().await;
        }
        let focused = window_focused().await;
        let elsewhere = !(fleet::is_open(state, &environment) && state.active_thread().peek().as_deref() == Some(alert.thread_id.as_str()));
        if in_app && focused && elsewhere {
            let level = match alert.level() {
                "success" => Level::Success,
                "error" => Level::Error,
                _ => Level::Warning,
            };
            notice::push_with(state, level, alert.title, alert.thread_title.clone(), Some(Follow::Thread { label: "Open thread".into(), environment: environment.clone(), thread_id: alert.thread_id.clone() }));
            return;
        }
        if !logic::has_desktop_notifications(mode) || focused {
            return;
        }
        let tag = alert.tag(environment.as_str());
        let shown = if DESKTOP {
            #[cfg(feature = "desktop")]
            {
                crate::desktop_shell::notify(alert.title, &alert.thread_title)
            }
            #[cfg(not(feature = "desktop"))]
            false
        } else if eval_page("return window.__t3Notify.permission();").join::<String>().await.is_ok_and(|permission| permission == "granted") {
            let (title, body, tag) = (json!(alert.title), json!(alert.thread_title), json!(tag));
            eval_page(&format!("return window.__t3Notify.show({tag}, {title}, {body});")).join::<bool>().await.unwrap_or(false)
        } else {
            false
        };
        if shown {
            let count = {
                let mut tags = pending.borrow_mut();
                tags.insert(alert.tag(environment.as_str()));
                tags.len()
            };
            show_badge(count);
        }
    });
}
