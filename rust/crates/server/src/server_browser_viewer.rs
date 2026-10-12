//! The people watching a server tab (the viewer half of apps/server/src/preview/ServerBrowser.ts): the screencast sent to each, its pacing, and
//! what a viewer's input does to the page once it has taken control.
//!
//! Each viewer has a session of its own on the page, so its screencast and its input stay apart from the agent's and from other viewers'. Frames
//! are acknowledged to Chromium only when the viewer acknowledges them, paced to about 30 per second; a viewer that stops reading loses its
//! backlog (a frame is dropped, state it cannot miss replaces the backlog) and is disconnected by the route when it falls too far behind.
use crate::{
    server_browser::{Core, ServerBrowser, ServerBrowserError, Tab, launch_error},
    server_browser_input::editing_command,
    server_browser_page::{EDITABLE_AT_POINT_SCRIPT, OpError, PageSession},
};
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, task::JoinHandle, time::Instant};

/// Chromium allows three unacked frames, so 100 ms pacing caps viewers near 30 fps.
const SCREENCAST_ACK_PACE: Duration = Duration::from_millis(100);
const SCREENCAST_SETTLE: Duration = Duration::from_millis(200);
/// Only scrolling triggers reduced-quality motion frames; animations stay sharp.
const SCREENCAST_MOTION_FRAMES: usize = 4;
const SCREENCAST_MOTION_WINDOW: Duration = Duration::from_millis(300);
const SCREENCAST_MOTION_QUALITY: u32 = 50;
pub const VIEWER_OUTPUT_LIMIT: usize = 64;
/// Copies reach the viewer only this soon after it last touched the page.
pub const CLIPBOARD_GESTURE: Duration = Duration::from_secs(5);

/// What the browser tells a viewer.
#[derive(Debug)]
pub enum ViewerOutput {
    Frame { data: Vec<u8>, ack: Ack },
    Viewport { width: u32, height: u32 },
    Control { can_operate: bool, controller: &'static str, generation: u64, dialog: Option<Value> },
    Probe { x: f64, y: f64, editable: bool },
    Clipboard { text: String },
    Pointer { phase: &'static str, x: f64, y: f64, sequence: u64 },
    FileChooser { id: String, multiple: bool, accept: String },
    FileChooserClosed { id: String },
    Download { id: String, file_name: String, size_bytes: u64 },
    Gone,
    /// The page is still open, but this connection to it ended; the viewer reconnects.
    Reconnect,
}

/// Releases Chromium to send the next frame, once the viewer has the one this goes with.
pub struct Ack(Option<BoxFuture<'static, ()>>);

impl Ack {
    pub fn none() -> Self {
        Self(None)
    }
    pub fn of(future: BoxFuture<'static, ()>) -> Self {
        Self(Some(future))
    }
    /// Runs the acknowledgement; a frame that was never shown still releases Chromium.
    pub fn release(mut self) {
        if let Some(future) = self.0.take() {
            tokio::spawn(future);
        }
    }
    /// The acknowledgement itself, to run where the caller wants to wait for it.
    pub fn into_future(mut self) -> BoxFuture<'static, ()> {
        self.0.take().unwrap_or_else(|| Box::pin(async {}))
    }
}
impl std::fmt::Debug for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ack")
    }
}

impl ViewerOutput {
    /// The JSON text a viewer reads for everything but a frame.
    pub fn to_json(&self) -> Option<Value> {
        Some(match self {
            Self::Frame { .. } | Self::Gone | Self::Reconnect => return None,
            Self::Viewport { width, height } => json!({"type": "viewport", "width": width, "height": height}),
            Self::Control { can_operate, controller, generation, dialog } => json!({"type": "control", "canOperate": can_operate, "controller": controller, "generation": generation, "dialog": dialog}),
            Self::Probe { x, y, editable } => json!({"type": "probe", "x": x, "y": y, "editable": editable}),
            Self::Clipboard { text } => json!({"type": "clipboard", "text": text}),
            Self::Pointer { phase, x, y, sequence } => json!({"type": "pointer", "phase": phase, "x": x, "y": y, "sequence": sequence}),
            Self::FileChooser { id, multiple, accept } => json!({"type": "fileChooser", "id": id, "multiple": multiple, "accept": accept}),
            Self::FileChooserClosed { id } => json!({"type": "fileChooserClosed", "id": id}),
            Self::Download { id, file_name, size_bytes } => json!({"type": "download", "id": id, "fileName": file_name, "sizeBytes": size_bytes}),
        })
    }
}

/// A viewer's pending output, bounded and dropping.
#[derive(Default)]
pub struct ViewerQueue {
    items: Mutex<VecDeque<ViewerOutput>>,
    wake: Notify,
    closed: AtomicBool,
}

impl ViewerQueue {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<ViewerOutput>> {
        self.items.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// Whether it was queued; a full queue takes nothing.
    pub fn offer(&self, item: ViewerOutput) -> Result<(), ViewerOutput> {
        let mut items = self.lock();
        if self.closed.load(Ordering::SeqCst) || items.len() >= VIEWER_OUTPUT_LIMIT {
            return Err(item);
        }
        items.push_back(item);
        drop(items);
        self.wake.notify_one();
        Ok(())
    }
    /// Takes everything queued, in order.
    pub fn clear(&self) -> Vec<ViewerOutput> {
        self.lock().drain(..).collect()
    }
    /// The next output, waiting for one; none once the queue is closed and empty.
    pub async fn next(&self) -> Option<ViewerOutput> {
        loop {
            let notified = self.wake.notified();
            if let Some(item) = self.lock().pop_front() {
                return Some(item);
            }
            if self.closed.load(Ordering::SeqCst) {
                return None;
            }
            notified.await;
        }
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        self.wake.notify_one();
    }
    pub fn len(&self) -> usize {
        self.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// What the screencast has been doing, which decides when a still is owed and when motion frames give way to sharp ones.
#[derive(Default)]
struct Motion {
    may_have_dropped: bool,
    moving: bool,
    recent: Vec<Instant>,
}

/// One person's view of a tab.
pub struct Viewer {
    pub(crate) id: String,
    pub(crate) can_operate: bool,
    queue: Arc<ViewerQueue>,
    session: PageSession,
    quality: u32,
    max_width: u32,
    max_height: u32,
    pressed_keys: Mutex<HashMap<String, (String, String)>>,
    pressed_buttons: Mutex<HashMap<&'static str, (f64, f64)>>,
    scrolled_at: Mutex<Option<Instant>>,
    /// Last input from this viewer; page copies reach its clipboard only right after.
    input_at: Mutex<Option<Instant>>,
    /// Panel bounds, retained in fixed mode; passive viewers never request a size.
    requested_size: Mutex<Option<(u32, u32, u64)>>,
    scale: Mutex<f64>,
    /// Orders the screencast's starts and stops.
    screencast: tokio::sync::Mutex<()>,
}

impl Viewer {
    /// How much output is waiting for the viewer.
    pub(crate) fn queued(&self) -> usize {
        self.queue.len()
    }
    pub(crate) fn requested_size(&self) -> Option<(u32, u32, u64)> {
        *self.requested_size.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    pub(crate) fn input_age(&self) -> Option<Duration> {
        self.input_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).map(|at| at.elapsed())
    }

    /// Queues output for the viewer. Dropped frames must still release Chromium; state a stalled viewer cannot miss replaces its backlog.
    pub(crate) fn push(&self, output: ViewerOutput, tab: &Tab) {
        let Err(output) = self.queue.offer(output) else { return };
        match output {
            ViewerOutput::Frame { ack, .. } => ack.release(),
            ViewerOutput::Gone | ViewerOutput::Control { .. } | ViewerOutput::FileChooser { .. } => {
                let dropped = self.queue.clear();
                let is_control = matches!(output, ViewerOutput::Control { .. });
                let _ = self.queue.offer(output);
                // The controller's open picker may have been in the dropped backlog.
                if is_control && tab.control.controller().as_deref() == Some(self.id.as_str()) && let Some(chooser) = Core::file_chooser_message(tab) {
                    let _ = self.queue.offer(chooser);
                }
                for item in dropped {
                    if let ViewerOutput::Frame { ack, .. } = item {
                        ack.release();
                    }
                }
            }
            _ => {}
        }
    }

    fn scale(&self) -> f64 {
        *self.scale.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// (Re)starts the screencast at `scale` of the viewer's size.
    pub(crate) async fn start_screencast(&self, tab: &Tab, scale: f64) {
        *self.scale.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = scale;
        let _ordered = self.screencast.lock().await;
        // A scaled capture is rendering; its resume starts the stream.
        if tab.capturing.load(Ordering::SeqCst) > 0 {
            return;
        }
        let _ = self.session.send("Page.stopScreencast", json!({})).await;
        let quality = if scale < 1.0 { self.quality.min(SCREENCAST_MOTION_QUALITY) } else { self.quality };
        let _ = self
            .session
            .send(
                "Page.startScreencast",
                json!({"format": "jpeg", "quality": quality, "maxWidth": (f64::from(self.max_width) * scale).round().max(1.0), "maxHeight": (f64::from(self.max_height) * scale).round().max(1.0)}),
            )
            .await;
    }

    pub(crate) async fn pause(&self) {
        let _ordered = self.screencast.lock().await;
        let _ = self.session.send("Page.stopScreencast", json!({})).await;
    }

    pub(crate) async fn resume(&self, tab: &Tab) {
        let scale = self.scale();
        self.start_screencast(tab, scale).await;
    }
}

/// The part of a viewer's connection the route reads and writes.
#[async_trait]
pub trait ViewerStream: Send + Sync {
    /// The next output; none once the viewer is closed.
    async fn next(&self) -> Option<ViewerOutput>;
    /// A message the viewer sent; ignored unless it may operate.
    async fn input(&self, message: Value);
    /// Ends the viewer: its control is released, its screencast stops.
    async fn close(&self);
}

/// An attached viewer.
pub struct ViewerHandle {
    core: Arc<Core>,
    tab: Arc<Tab>,
    viewer: Arc<Viewer>,
    queue: Arc<ViewerQueue>,
    feeder: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        // A connection that ends without saying so still leaves the tab.
        if !self.closed.swap(true, Ordering::SeqCst) {
            let (core, tab, viewer, feeder) = (self.core.clone(), self.tab.clone(), self.viewer.clone(), self.feeder.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take());
            tokio::spawn(async move { detach_viewer(core, tab, viewer, feeder).await });
        }
    }
}

async fn detach_viewer(core: Arc<Core>, tab: Arc<Tab>, viewer: Arc<Viewer>, feeder: Option<JoinHandle<()>>) {
    if let Some(feeder) = feeder {
        feeder.abort();
    }
    let released = viewer.clone();
    let _ = tab.control.disconnect::<OpError, _, _>(&viewer.id, move || async move {
        release_viewer_input(&released).await;
        Ok(())
    })
    .await;
    tab.viewers.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).retain(|candidate| candidate.id != viewer.id);
    core.broadcast_control(&tab);
    core.report_live_tabs();
    let _ = viewer.session.send("Page.stopScreencast", json!({})).await;
    viewer.session.detach();
    viewer.queue.close();
}

#[async_trait]
impl ViewerStream for ViewerHandle {
    async fn next(&self) -> Option<ViewerOutput> {
        self.queue.next().await
    }

    async fn input(&self, message: Value) {
        let (core, tab, viewer) = (&self.core, &self.tab, &self.viewer);
        if !viewer.can_operate || !message.is_object() {
            return;
        }
        let kind = message["type"].as_str().unwrap_or_default();
        let result: Result<(), OpError> = async {
            match kind {
                "takeControl" => {
                    let taking = tab.control.take(&viewer.id);
                    core.broadcast_control(tab);
                    taking.await?;
                    core.push_file_chooser(tab);
                }
                "releaseControl" => {
                    let released = viewer.clone();
                    let releasing = tab.control.release::<OpError, _, _>(&viewer.id, move || async move {
                        release_viewer_input(&released).await;
                        Ok(())
                    });
                    core.broadcast_control(tab);
                    releasing.await?;
                }
                "dialog" if tab.control.controller().as_deref() == Some(viewer.id.as_str()) && message["accept"].is_boolean() => {
                    let input = serde_json::from_value(json!({"accept": message["accept"], "promptText": message["promptText"].as_str()})).unwrap_or_default();
                    core.resolve_dialog(tab, &input).await?;
                }
                _ => {
                    let (core, tab, viewer, message) = (core.clone(), tab.clone(), viewer.clone(), message.clone());
                    let running = tab.clone();
                    running.control.human::<(), OpError, _, _>(&viewer.id.clone(), move || async move { dispatch_viewer_input(&core, &tab, &viewer, &message).await }).await?;
                }
            }
            Ok(())
        }
        .await;
        // Rejected ownership cannot mutate the page; refresh the viewer's controls.
        if result.is_err() {
            core.broadcast_control(tab);
        }
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let feeder = self.feeder.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        detach_viewer(self.core.clone(), self.tab.clone(), self.viewer.clone(), feeder).await;
    }
}

/// Lets go of whatever the viewer still holds down in the page.
async fn release_viewer_input(viewer: &Viewer) {
    let keys: Vec<(String, String)> = viewer.pressed_keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain().map(|(_, key)| key).collect();
    for (key, code) in keys {
        let _ = viewer.session.send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": key, "code": code})).await;
    }
    let buttons: Vec<(&'static str, (f64, f64))> = viewer.pressed_buttons.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain().collect();
    for (button, (x, y)) in buttons {
        let _ = viewer.session.send("Input.dispatchMouseEvent", json!({"type": "mouseReleased", "button": button, "x": x, "y": y, "buttons": 0, "clickCount": 1})).await;
    }
    *viewer.requested_size.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn num(value: &Value, fallback: f64) -> f64 {
    value.as_f64().filter(|number| number.is_finite()).unwrap_or(fallback)
}

fn modifiers_of(message: &Value) -> u32 {
    let value = num(&message["modifiers"], 0.0);
    if value.fract() == 0.0 && (0.0..16.0).contains(&value) { value as u32 } else { 0 }
}

/// What a viewer's input does to the page, once it controls the tab.
async fn dispatch_viewer_input(core: &Arc<Core>, tab: &Arc<Tab>, viewer: &Arc<Viewer>, message: &Value) -> Result<(), OpError> {
    *viewer.input_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    let session = &viewer.session;
    let modifiers = modifiers_of(message);
    let send = |method: &'static str, params: Value| async move { session.send(method, params).await.map_err(crate::server_browser_page::map_cdp) };
    match message["type"].as_str().unwrap_or_default() {
        "mouse" => {
            let action = message["action"].as_str().unwrap_or_default();
            let kind = match action {
                "down" => "mousePressed",
                "up" => "mouseReleased",
                _ => "mouseMoved",
            };
            let button: &'static str = match message["button"].as_str() {
                Some("left") => "left",
                Some("middle") => "middle",
                Some("right") => "right",
                _ => "none",
            };
            send(
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind,
                    "x": num(&message["x"], 0.0),
                    "y": num(&message["y"], 0.0),
                    "button": button,
                    "buttons": num(&message["buttons"], 0.0),
                    "clickCount": num(&message["clickCount"], if kind == "mouseMoved" { 0.0 } else { 1.0 }),
                    "modifiers": modifiers,
                }),
            )
            .await?;
            if button != "none" {
                let mut pressed = viewer.pressed_buttons.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if action == "down" {
                    pressed.insert(button, (num(&message["x"], 0.0), num(&message["y"], 0.0)));
                } else if action == "up" {
                    pressed.remove(button);
                }
            }
        }
        "wheel" => {
            *viewer.scrolled_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
            send(
                "Input.dispatchMouseEvent",
                json!({"type": "mouseWheel", "x": num(&message["x"], 0.0), "y": num(&message["y"], 0.0), "deltaX": num(&message["deltaX"], 0.0), "deltaY": num(&message["deltaY"], 0.0), "modifiers": modifiers}),
            )
            .await?;
        }
        "key" => {
            let key = message["key"].as_str().unwrap_or_default();
            let code = message["code"].as_str().unwrap_or_default();
            let text = message["text"].as_str().filter(|text| !text.is_empty());
            let remember = if code.is_empty() { key } else { code };
            if message["action"] == "up" {
                send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": key, "code": code, "modifiers": modifiers})).await?;
                viewer.pressed_keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(remember);
                return Ok(());
            }
            let fallback = if key.chars().count() == 1 { key.to_uppercase().chars().next().map_or(0.0, |c| f64::from(c as u32)) } else { 0.0 };
            let mut params = json!({"type": if text.is_some() { "keyDown" } else { "rawKeyDown" }, "key": key, "code": code, "modifiers": modifiers, "windowsVirtualKeyCode": num(&message["keyCode"], fallback)});
            if let Some(text) = text {
                params["text"] = json!(text);
                params["unmodifiedText"] = json!(text);
            }
            if let Some(commands) = editing_command(key, modifiers) {
                params["commands"] = json!(commands);
            }
            send("Input.dispatchKeyEvent", params).await?;
            viewer.pressed_keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(remember.to_owned(), (key.to_owned(), code.to_owned()));
        }
        "text" => {
            if let Some(text) = message["text"].as_str().filter(|text| !text.is_empty()) {
                send("Input.insertText", json!({"text": text.chars().take(10_000).collect::<String>()})).await?;
            }
        }
        "resize" => {
            let width = (num(&message["width"], 0.0).round()).min(3840.0);
            let height = (num(&message["height"], 0.0).round()).min(2160.0);
            if width < 100.0 || height < 100.0 {
                return Ok(());
            }
            let order = core.viewer_resize_order.fetch_add(1, Ordering::SeqCst) + 1;
            *viewer.requested_size.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((width as u32, height as u32, order));
            if tab.state().setting["_tag"] != "fill" {
                return Ok(());
            }
            if tab.page.viewport() == Some((width as u32, height as u32)) {
                return Ok(());
            }
            tab.page.set_viewport(width as u32, height as u32).await?;
            core.broadcast_viewport(tab);
        }
        "viewport" => {
            let setting: Value = message["setting"].clone();
            let resize = serde_json::from_value(json!({"threadId": tab.thread_id, "tabId": tab.tab_id, "viewport": setting.clone()})).map_err(|error| OpError::execution(error.to_string()))?;
            core.manager.resize(resize).map_err(|error| OpError::execution(error.message()))?;
            core.apply_setting(tab, setting).await?;
        }
        "navigate" => {
            if let Some(url) = message["url"].as_str() {
                let url = crate::preview_manager::normalize_preview_url(url).map_err(|error| OpError::execution(error.message()))?;
                tab.page.goto(&url, crate::server_browser_page::Readiness::Commit, crate::server_browser::NAVIGATION_TIMEOUT).await?;
            }
        }
        "history" => {
            tab.page.go_history(if num(&message["delta"], 0.0) < 0.0 { -1 } else { 1 }, crate::server_browser::NAVIGATION_TIMEOUT).await?;
        }
        "reload" => {
            // A hard reload fetches everything again, as Chrome's Shift+Reload does.
            tab.page.reload(message["ignoreCache"] == true, crate::server_browser::NAVIGATION_TIMEOUT).await?;
        }
        "probe" => {
            let (x, y) = (num(&message["x"], 0.0), num(&message["y"], 0.0));
            let result = send("Runtime.evaluate", json!({"expression": format!("({EDITABLE_AT_POINT_SCRIPT})({x}, {y})"), "returnByValue": true})).await?;
            viewer.push(ViewerOutput::Probe { x, y, editable: result["result"]["value"] == true }, tab);
        }
        _ => {}
    }
    Ok(())
}

impl Core {
    /// Attaches a viewer to a tab: it gets the tab's control state, its size and file picker, then its screencast.
    pub(crate) async fn attach_viewer(self: &Arc<Self>, thread_id: &str, tab_id: &str, max_width: u32, max_height: u32, quality: u32, can_operate: bool) -> Result<ViewerHandle, ServerBrowserError> {
        let tab = self.find_tab(thread_id, tab_id).await?;
        let session = tab.page.new_session().await.map_err(launch_error)?;
        let session_id = session.id.clone();
        let mut frames = session.connection().events(Some(&session_id));
        let poster = session.connection().poster();
        let queue = Arc::new(ViewerQueue::default());
        let quality = quality.clamp(1, 100);
        let viewer = Arc::new(Viewer {
            id: uuid::Uuid::new_v4().to_string(),
            can_operate,
            queue: queue.clone(),
            session,
            quality,
            max_width,
            max_height,
            pressed_keys: Mutex::default(),
            pressed_buttons: Mutex::default(),
            scrolled_at: Mutex::default(),
            input_at: Mutex::default(),
            requested_size: Mutex::default(),
            scale: Mutex::new(1.0),
            screencast: tokio::sync::Mutex::new(()),
        });
        tab.viewers.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(viewer.clone());
        self.report_live_tabs();
        if can_operate && tab.control.agent_id().is_none() && tab.control.controller().is_none() {
            let _ = tab.control.take(&viewer.id).await;
        }
        self.broadcast_control(&tab);
        self.push_file_chooser(&tab);
        // Chromium may drop the final frame during a burst; a still is sent when it settles.
        let started = Arc::new(AtomicBool::new(false));
        let feeder = {
            let (core, tab, viewer, started) = (self.clone(), tab.clone(), viewer.clone(), started.clone());
            tokio::spawn(async move {
                let in_flight = Arc::new(AtomicUsize::new(0));
                let state = Arc::new(Mutex::new(Motion::default()));
                let mut settle: Option<JoinHandle<()>> = None;
                while let Some(event) = frames.recv().await {
                    if event.method != "Page.screencastFrame" {
                        continue;
                    }
                    started.store(true, Ordering::SeqCst);
                    let arrived = Instant::now();
                    let restart = {
                        let mut motion = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if in_flight.load(Ordering::SeqCst) > 0 {
                            motion.may_have_dropped = true;
                        }
                        motion.recent.push(arrived);
                        motion.recent.retain(|at| *at + SCREENCAST_MOTION_WINDOW >= arrived);
                        let scrolled = *viewer.scrolled_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if !motion.moving && motion.recent.len() >= SCREENCAST_MOTION_FRAMES && scrolled.is_some_and(|at| arrived.saturating_duration_since(at) < SCREENCAST_MOTION_WINDOW) {
                            motion.moving = true;
                            Some(0.5)
                        } else if motion.moving && scrolled.is_none_or(|at| arrived.saturating_duration_since(at) > SCREENCAST_SETTLE) {
                            // The page keeps animating after the scroll; its own frames are sharp again.
                            motion.moving = false;
                            motion.recent.clear();
                            Some(1.0)
                        } else {
                            None
                        }
                    };
                    in_flight.fetch_add(1, Ordering::SeqCst);
                    if let Some(scale) = restart {
                        let (viewer, tab) = (viewer.clone(), tab.clone());
                        tokio::spawn(async move { viewer.start_screencast(&tab, scale).await });
                    }
                    if let Some(timer) = settle.take() {
                        timer.abort();
                    }
                    let (settle_core, settle_tab, settle_viewer, settle_state) = (core.clone(), tab.clone(), viewer.clone(), state.clone());
                    settle = Some(tokio::spawn(async move {
                        tokio::time::sleep(SCREENCAST_SETTLE).await;
                        let was_moving = {
                            let mut motion = settle_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                            if !motion.moving && !motion.may_have_dropped {
                                return;
                            }
                            motion.may_have_dropped = false;
                            let was_moving = motion.moving;
                            if was_moving {
                                motion.moving = false;
                                motion.recent.clear();
                            }
                            was_moving
                        };
                        if was_moving {
                            let (viewer, tab) = (settle_viewer.clone(), settle_tab.clone());
                            tokio::spawn(async move { viewer.start_screencast(&tab, 1.0).await });
                        }
                        push_still(&settle_core, &settle_tab, &settle_viewer).await;
                    }));
                    let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, event.params["data"].as_str().unwrap_or_default()).unwrap_or_default();
                    let (poster, session_id, ack_id, in_flight) = (poster.clone(), session_id.clone(), event.params["sessionId"].clone(), in_flight.clone());
                    let ack = Ack::of(Box::pin(async move {
                        tokio::time::sleep_until(arrived + SCREENCAST_ACK_PACE).await;
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        poster.post("Page.screencastFrameAck", json!({"sessionId": ack_id}), Some(&session_id));
                    }));
                    viewer.push(ViewerOutput::Frame { data, ack }, &tab);
                }
            })
        };
        self.broadcast_viewport(&tab);
        viewer.start_screencast(&tab, 1.0).await;
        // An idle page does not repaint for a new screencast, so the viewer starts from a still unless a live frame beat it.
        if !started.load(Ordering::SeqCst) {
            push_still(self, &tab, &viewer).await;
        }
        Ok(ViewerHandle { core: self.clone(), tab, viewer, queue, feeder: Mutex::new(Some(feeder)), closed: AtomicBool::new(false) })
    }
}

/// A full-scale still of the page: a scaled capture would flash in every other viewer.
async fn push_still(core: &Arc<Core>, tab: &Arc<Tab>, viewer: &Arc<Viewer>) {
    let quality = viewer.quality;
    let captured = core.with_capture_lock(tab, async { tab.page.capture_viewport(Some(&viewer.session), "jpeg", Some(quality), 1.0).await }).await;
    if let Ok(data) = captured && let Ok(data) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data) {
        viewer.push(ViewerOutput::Frame { data, ack: Ack::none() }, tab);
    }
}

/// What the stream routes use of the server browser.
#[async_trait]
pub trait ServerBrowserApi: Send + Sync {
    async fn attach_viewer(&self, thread_id: &str, tab_id: &str, max_width: u32, max_height: u32, quality: u32, can_operate: bool) -> Result<Box<dyn ViewerStream>, ServerBrowserError>;
    async fn answer_file_chooser(&self, thread_id: &str, tab_id: &str, chooser_id: &str, files: Vec<crate::server_browser::UploadedFile>) -> bool;
    fn open_download(&self, thread_id: &str, tab_id: &str, download_id: &str) -> Option<(PathBuf, String)>;
    async fn clear_profile(&self, profile_id: &str) -> std::io::Result<()>;
}

#[async_trait]
impl ServerBrowserApi for ServerBrowser {
    async fn attach_viewer(&self, thread_id: &str, tab_id: &str, max_width: u32, max_height: u32, quality: u32, can_operate: bool) -> Result<Box<dyn ViewerStream>, ServerBrowserError> {
        Ok(Box::new(self.0.attach_viewer(thread_id, tab_id, max_width, max_height, quality, can_operate).await?))
    }
    async fn answer_file_chooser(&self, thread_id: &str, tab_id: &str, chooser_id: &str, files: Vec<crate::server_browser::UploadedFile>) -> bool {
        ServerBrowser::answer_file_chooser(self, thread_id, tab_id, chooser_id, files).await
    }
    fn open_download(&self, thread_id: &str, tab_id: &str, download_id: &str) -> Option<(PathBuf, String)> {
        ServerBrowser::open_download(self, thread_id, tab_id, download_id)
    }
    async fn clear_profile(&self, profile_id: &str) -> std::io::Result<()> {
        ServerBrowser::clear_profile(self, profile_id).await
    }
}

#[cfg(test)]
#[path = "server_browser_viewer_tests.rs"]
mod tests;
