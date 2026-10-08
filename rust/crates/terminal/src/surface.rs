//! The DOM owns events and geometry only; terminal/input/canvas policy stays
//! in Rust and is shared by browser builds and native client WebViews.
use crate::{
    core::{
        TerminalCore,
        selection::{Point, Range},
    },
    keyboard::KeyInput,
    model::{Snapshot, Theme},
    renderer::{self, Metrics, Paint, WebCanvas},
    runtime::{Result, error},
};
use js_sys::Function;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{
    CanvasRenderingContext2d, ClipboardEvent, CompositionEvent, Event, EventTarget,
    HtmlCanvasElement, HtmlElement, HtmlTextAreaElement, InputEvent, KeyboardEvent, PointerEvent,
    ResizeObserver, WheelEvent,
};
const DEFAULT_FONT: &str = "\"SF Mono\", \"SFMono-Regular\", Menlo, Consolas, \"Liberation Mono\", \"Symbols Nerd Font Mono\", \"Symbols Nerd Font\", \"JetBrainsMono Nerd Font\", \"JetBrainsMono NF\", \"FiraCode Nerd Font\", \"Hack Nerd Font\", \"MesloLGS NF\", \"CaskaydiaCove Nerd Font\", \"PowerlineSymbols\", monospace";
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Options {
    #[serde(default)]
    theme: Theme,
    #[serde(default)]
    read_only: bool,
    #[serde(default = "font_size")]
    font_size: f64,
}
fn font_size() -> f64 {
    12.0
}
struct Listener {
    target: EventTarget,
    name: &'static str,
    callback: Closure<dyn Fn(Event)>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self
            .target
            .remove_event_listener_with_callback(self.name, self.callback.as_ref().unchecked_ref());
    }
}
struct State {
    core: TerminalCore,
    host: HtmlElement,
    canvas: HtmlCanvasElement,
    input: HtmlTextAreaElement,
    context: CanvasRenderingContext2d,
    scrollbar: HtmlElement,
    thumb: HtmlElement,
    scroll_pointer: Option<i32>,
    scroll_offset: f64,
    last_origin: f64,
    callback: Function,
    outbox: Rc<RefCell<Vec<Value>>>,
    theme: Theme,
    metrics: Metrics,
    font_size: f64,
    visible: bool,
    read_only: bool,
    writable: Rc<Cell<bool>>,
    disposed: bool,
    focused: bool,
    full: bool,
    cursor_on: bool,
    frame: Option<i32>,
    blink: Option<i32>,
    resize_timer: Option<i32>,
    composition_timer: Option<i32>,
    frame_callback: Option<Closure<dyn Fn(f64)>>,
    blink_callback: Option<Closure<dyn Fn()>>,
    resize_callback: Option<Closure<dyn Fn()>>,
    composition_callback: Option<Closure<dyn Fn()>>,
    snapshot: Option<Snapshot>,
    previous_cursor: Option<i32>,
    grid: (u16, u16),
    mount_height: f64,
    composing: bool,
    composition_suppress: Option<String>,
    suppressed: BTreeSet<String>,
    paste_token: u64,
    copy_token: u64,
    copy_clear: bool,
    mac: bool,
    wheel_remainder: f64,
    layout_map: Option<JsValue>,
    anchor: Option<Point>,
    selection_base: Option<Range>,
    selection_mode: u32,
    selection_end: Option<Point>,
    selection_moved: bool,
    clicks: Option<crate::surface_policy::ClickSequence>,
    selection_scroll: i32,
    selection_timer: Option<i32>,
    selection_pointer: Option<(f64, f64)>,
    selection_callback: Option<Closure<dyn Fn()>>,
    pointer: Option<i32>,
    mouse_button: Option<u32>,
    last_mouse: String,
    mouse_tracking: bool,
}
impl State {
    fn queue(&self, value: Value) {
        self.outbox.borrow_mut().push(value)
    }
    fn send(&mut self, data: String) {
        if !self.read_only && !data.is_empty() {
            self.queue(json!({"type":"write","data":data}));
        }
    }
    fn fail(&self, cause: JsValue) {
        self.queue(json!({"type":"error","message":format!("{cause:?}")}));
    }
    fn request_frame(&mut self) {
        if self.disposed || !self.visible || self.frame.is_some() {
            return;
        }
        if let (Some(window), Some(callback)) = (web_sys::window(), self.frame_callback.as_ref()) {
            self.frame = window
                .request_animation_frame(callback.as_ref().unchecked_ref())
                .ok();
        }
    }
    fn cancel_blink(&mut self) {
        if let (Some(window), Some(id)) = (web_sys::window(), self.blink.take()) {
            window.clear_timeout_with_handle(id)
        }
    }
    fn cancel_frame(&mut self) {
        if let (Some(window), Some(id)) = (web_sys::window(), self.frame.take()) {
            let _ = window.cancel_animation_frame(id);
        }
        self.cancel_blink();
    }
    fn fit(&mut self) -> Result<()> {
        if !self.visible || self.disposed {
            return Ok(());
        }
        let width = self.host.client_width() as f64;
        let height = self.host.client_height() as f64;
        if width <= 0.0 || height <= 0.0 {
            self.cancel_frame();
            self.full = true;
            return Ok(());
        }
        let window = web_sys::window().ok_or_else(|| error("Terminal window unavailable"))?;
        let ratio = window.device_pixel_ratio().max(1.0);
        let pw = (width * ratio).round().max(1.0) as u32;
        let ph = (height * ratio).round().max(1.0) as u32;
        if self.canvas.width() != pw || self.canvas.height() != ph || self.snapshot.is_none() {
            self.canvas.set_width(pw);
            self.canvas.set_height(ph);
            self.context
                .set_transform(ratio, 0.0, 0.0, ratio, 0.0, 0.0)?;
            self.full = true;
        }
        self.mount_height = height;
        let (cols, rows) = renderer::grid_size(width, height, self.metrics, 4.0);
        let grid = (cols.min(65535.0) as u16, rows.min(65535.0) as u16);
        if grid != self.grid {
            self.grid = grid;
            self.core.resize(
                grid.0,
                grid.1,
                self.metrics.width.round().max(1.0) as u32,
                self.metrics.height.round().max(1.0) as u32,
            )?;
            self.full = true;
            if let Some(id) = self.resize_timer.take() {
                window.clear_timeout_with_handle(id)
            }
            if let Some(callback) = self.resize_callback.as_ref() {
                self.resize_timer = window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        callback.as_ref().unchecked_ref(),
                        100,
                    )
                    .ok();
            }
        }
        // ResizeObserver runs before paint; configure and paint together, not one
        // stretched canvas frame followed by a later RAF.
        self.draw()
    }
    fn draw(&mut self) -> Result<()> {
        if self.disposed
            || !self.visible
            || self.host.client_width() <= 0
            || self.host.client_height() <= 0
        {
            return Ok(());
        }
        let snapshot = self.core.update()?;
        let anchor = self
            .core
            .scrollbar()?
            .is_some_and(|scroll| scroll.total > scroll.len);
        let origin = if anchor {
            4.0 + (self.mount_height - 8.0 - snapshot.rows as f64 * self.metrics.height).max(0.0)
        } else {
            4.0
        };
        if self.last_origin != origin {
            self.full = true;
            self.last_origin = origin;
        }
        self.update_scrollbar()?;
        renderer::paint(
            &mut WebCanvas(self.context.clone()),
            &snapshot,
            self.core.rows(),
            Paint {
                metrics: self.metrics,
                font_size: self.font_size,
                font_family: DEFAULT_FONT,
                padding: 4.0,
                force_full: self.full,
                cursor_on: self.cursor_on,
                previous_cursor_y: self.previous_cursor,
                focused: self.focused,
                selection_background: self.theme.selection_background.as_deref(),
                hovered_link: None,
                origin_y: Some(origin),
            },
        );
        self.input.style().set_property(
            "left",
            &format!(
                "{}px",
                4.0 + snapshot.cursor_x.max(0) as f64 * self.metrics.width
            ),
        )?;
        self.input.style().set_property(
            "top",
            &format!(
                "{}px",
                origin + snapshot.cursor_y.max(0) as f64 * self.metrics.height
            ),
        )?;
        self.previous_cursor = Some(snapshot.cursor_y);
        self.full = false;
        self.cancel_blink();
        let reduced = web_sys::window()
            .and_then(|window| {
                window
                    .match_media("(prefers-reduced-motion: reduce)")
                    .ok()
                    .flatten()
            })
            .is_some_and(|query| query.matches());
        if self.focused && snapshot.cursor_blinking && snapshot.cursor_visible && !reduced {
            if let (Some(window), Some(callback)) =
                (web_sys::window(), self.blink_callback.as_ref())
            {
                self.blink = window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        callback.as_ref().unchecked_ref(),
                        500,
                    )
                    .ok();
            }
        }
        self.snapshot = Some(snapshot);
        Ok(())
    }
    fn update_scrollbar(&self) -> Result<()> {
        let Some(scroll) = self.core.scrollbar()? else {
            self.scrollbar.set_hidden(true);
            return Ok(());
        };
        let total = scroll.total as f64;
        let len = scroll.len.min(scroll.total) as f64;
        let height = self.scrollbar.client_height() as f64;
        let max = (total - len).max(0.0);
        if max == 0.0 || len == 0.0 {
            self.scrollbar.set_hidden(true);
            return Ok(());
        }
        self.scrollbar.set_hidden(false);
        let height = if height == 0.0 {
            (self.mount_height - 8.0).max(0.0)
        } else {
            height
        };
        let thumb = (height * len / total).max(18.0).min(height);
        let travel = (height - thumb).max(0.0);
        self.scrollbar.set_attribute("aria-valuemin", "0")?;
        self.scrollbar
            .set_attribute("aria-valuemax", &max.to_string())?;
        self.scrollbar
            .set_attribute("aria-valuenow", &scroll.offset.to_string())?;
        self.thumb
            .style()
            .set_property("height", &format!("{thumb}px"))?;
        self.thumb.style().set_property(
            "transform",
            &format!(
                "translateY({}px)",
                travel * (scroll.offset as f64 / max).clamp(0.0, 1.0)
            ),
        )?;
        Ok(())
    }
    fn scroll_pointer_to(&mut self, y: f64) -> Result<()> {
        let Some(scroll) = self.core.scrollbar()? else {
            return Ok(());
        };
        let bounds = self.scrollbar.get_bounding_client_rect();
        let height = bounds.height();
        let total = scroll.total as f64;
        let len = scroll.len.min(scroll.total) as f64;
        let max = total - len;
        if total == 0.0 || max == 0.0 {
            return Ok(());
        }
        let thumb = (height * len / total).max(18.0).min(height);
        let travel = (height - thumb).max(0.0);
        let offset = if travel == 0.0 {
            0.0
        } else {
            ((y - bounds.top() - self.scroll_offset).clamp(0.0, travel) / travel * max).round()
        };
        self.core.scroll((offset - scroll.offset as f64) as i32)?;
        self.full = true;
        Ok(())
    }
    fn point(&self, event: &PointerEvent) -> Point {
        self.point_at(event.client_x() as f64, event.client_y() as f64)
    }
    fn point_at(&self, x: f64, y: f64) -> Point {
        let bounds = self.canvas.get_bounding_client_rect();
        let rows = self.grid.1.max(1);
        let cols = self.grid.0.max(1);
        let anchor = self
            .core
            .scrollbar()
            .ok()
            .flatten()
            .is_some_and(|s| s.total > s.len);
        let origin = if anchor {
            4.0 + (self.mount_height - 8.0 - rows as f64 * self.metrics.height).max(0.0)
        } else {
            4.0
        };
        Point {
            x: ((x - bounds.left() - 4.0) / self.metrics.width)
                .floor()
                .clamp(0.0, cols as f64 - 1.0) as u16,
            y: ((y - bounds.top() - origin) / self.metrics.height)
                .floor()
                .clamp(0.0, rows as f64 - 1.0) as u32,
            tag: 1,
        }
    }
    fn extend_selection(&mut self, point: Point) -> Result<()> {
        let Some(anchor) = self.anchor else {
            return Ok(());
        };
        let Some(cell) = self.core.convert_point(point, 2)? else {
            return Ok(());
        };
        self.selection_moved = true;
        self.selection_end = Some(point);
        let range = if self.selection_mode > 1 {
            self.core
                .select_at(point.x, point.y, self.selection_mode >= 3)?
        } else {
            None
        };
        let before = self.selection_base.is_some_and(|base| {
            cell.y < base.start.y || (cell.y == base.start.y && cell.x < base.start.x)
        });
        let anchor = self
            .selection_base
            .map(|base| if before { base.end } else { base.start })
            .map(|p| Point {
                x: p.x,
                y: p.y,
                tag: 2,
            })
            .unwrap_or(anchor);
        let end = range
            .map(|range| {
                if before {
                    range.screen.start
                } else {
                    range.screen.end
                }
            })
            .unwrap_or(cell);
        self.anchor = Some(anchor);
        self.core.set_selection(
            anchor,
            Point {
                x: end.x,
                y: end.y,
                tag: 2,
            },
        )?;
        self.full = true;
        Ok(())
    }
    fn selection_autoscroll(&mut self, rows: i32) {
        self.selection_scroll = rows;
        let Some(window) = web_sys::window() else {
            return;
        };
        if rows == 0 {
            if let Some(timer) = self.selection_timer.take() {
                window.clear_interval_with_handle(timer)
            }
        } else if self.selection_timer.is_none() {
            if let Some(callback) = self.selection_callback.as_ref() {
                self.selection_timer = window
                    .set_interval_with_callback_and_timeout_and_arguments_0(
                        callback.as_ref().unchecked_ref(),
                        80,
                    )
                    .ok();
            }
        }
    }
    fn mouse(&mut self, event: &PointerEvent, action: &str, button: Option<u32>) -> Result<()> {
        let bounds = self.canvas.get_bounding_client_rect();
        let input = json!({"action":action,"button":button,"mods":u32::from(event.shift_key())|u32::from(event.ctrl_key())<<1|u32::from(event.alt_key())<<2|u32::from(event.meta_key())<<3,
   "x":event.client_x() as f64-bounds.left(),"y":event.client_y() as f64-bounds.top(),"screenWidth":bounds.width(),"screenHeight":bounds.height(),
   "cellWidth":self.metrics.width,"cellHeight":self.metrics.height,"paddingLeft":4,"paddingRight":4,"paddingTop":4,"paddingBottom":4,"anyButtonPressed":event.buttons()!=0});
        self.synchronize_mouse()?;
        let data = self.core.encode_mouse(&input.to_string())?;
        let decision = crate::surface_policy::mouse_data(action, &data, &self.last_mouse);
        self.last_mouse = decision.next_motion_data;
        if decision.send {
            self.send(data)
        }
        Ok(())
    }
    fn synchronize_mouse(&mut self) -> Result<()> {
        let tracking = self.core.mode(1003)?;
        let decision =
            crate::surface_policy::mouse_tracking(self.mouse_tracking, tracking, &self.last_mouse);
        self.mouse_tracking = decision.tracking;
        self.last_mouse = decision.motion_data;
        Ok(())
    }
    fn clear_composition(&mut self) {
        self.composition_suppress = None;
        if let (Some(window), Some(timer)) = (web_sys::window(), self.composition_timer.take()) {
            window.clear_timeout_with_handle(timer);
        }
    }
}
fn dispatch(state: &Rc<RefCell<State>>) {
    let (callback, outbox) = {
        let state = state.borrow();
        (state.callback.clone(), state.outbox.clone())
    };
    // Application callbacks may update/detach the terminal, so release the state
    // and queue borrows before crossing back into the host UI.
    let pending = std::mem::take(&mut *outbox.borrow_mut());
    for value in pending {
        if value["type"] == "__focus" {
            let input = state.borrow().input.clone();
            let _ = input.focus();
            continue;
        }
        if value["type"] == "write" && state.borrow().read_only {
            continue;
        }
        let _ = callback.call1(&JsValue::UNDEFINED, &JsValue::from_str(&value.to_string()));
    }
}
fn listen(
    state: &Rc<RefCell<State>>,
    target: EventTarget,
    name: &'static str,
    handler: impl Fn(&mut State, Event) -> Result<()> + 'static,
) -> Result<Listener> {
    let weak = Rc::downgrade(state);
    let callback = Closure::wrap(Box::new(move |event: Event| {
        if let Some(state) = weak.upgrade() {
            {
                let mut state = state.borrow_mut();
                if state.disposed {
                    return;
                }
                if let Err(cause) = handler(&mut state, event) {
                    state.fail(cause)
                }
                state.request_frame();
            }
            dispatch(&state);
        }
    }) as Box<dyn Fn(Event)>);
    let options = web_sys::AddEventListenerOptions::new();
    options.set_passive(false);
    target.add_event_listener_with_callback_and_add_event_listener_options(
        name,
        callback.as_ref().unchecked_ref(),
        &options,
    )?;
    Ok(Listener {
        target,
        name,
        callback,
    })
}
#[wasm_bindgen]
pub struct TerminalSurface {
    state: Rc<RefCell<State>>,
    listeners: Vec<Listener>,
    observer: ResizeObserver,
    _resize: Closure<dyn Fn()>,
    _writer: Closure<dyn Fn(String)>,
}
fn timer(
    state: &Rc<RefCell<State>>,
    handler: impl Fn(&mut State) -> Result<()> + 'static,
) -> Closure<dyn Fn()> {
    let weak = Rc::downgrade(state);
    Closure::wrap(Box::new(move || {
        if let Some(state) = weak.upgrade() {
            {
                let mut state = state.borrow_mut();
                if state.disposed {
                    return;
                }
                if let Err(cause) = handler(&mut state) {
                    state.fail(cause)
                }
            }
            dispatch(&state);
        }
    }) as Box<dyn Fn()>)
}
struct MountGuard {
    host: HtmlElement,
    nodes: Vec<web_sys::Element>,
    committed: bool,
}
impl Drop for MountGuard {
    fn drop(&mut self) {
        if !self.committed {
            for node in &self.nodes {
                if node.parent_element().as_ref()
                    == Some(self.host.unchecked_ref::<web_sys::Element>())
                {
                    node.remove();
                }
            }
        }
    }
}
#[wasm_bindgen]
pub async fn mount_terminal(
    host: HtmlElement,
    options_json: &str,
    callback: Function,
) -> Result<TerminalSurface> {
    let options: Options = serde_json::from_str(options_json).map_err(|e| error(e.to_string()))?;
    let document = host
        .owner_document()
        .ok_or_else(|| error("Terminal document unavailable"))?;
    let canvas: HtmlCanvasElement = document.create_element("canvas")?.dyn_into()?;
    canvas.set_attribute("aria-hidden", "true")?;
    canvas
        .style()
        .set_css_text("display:block;width:100%;height:100%;cursor:text;touch-action:none");
    let input: HtmlTextAreaElement = document.create_element("textarea")?.dyn_into()?;
    input.set_attribute("aria-label", "Terminal input")?;
    input.set_attribute("autocapitalize", "off")?;
    input.set_attribute("autocomplete", "off")?;
    input.set_spellcheck(false);
    input.style().set_css_text("position:absolute;left:4px;top:4px;width:1px;height:1px;opacity:0;padding:0;border:0;resize:none;pointer-events:none");
    let scrollbar: HtmlElement = document.create_element("div")?.dyn_into()?;
    scrollbar.set_attribute("role", "scrollbar")?;
    scrollbar.set_attribute("aria-label", "Terminal scrollback")?;
    scrollbar.set_attribute("aria-orientation", "vertical")?;
    scrollbar.set_tab_index(0);
    scrollbar.set_hidden(true);
    scrollbar.style().set_css_text("position:absolute;right:1px;top:4px;bottom:4px;width:10px;cursor:default;touch-action:none");
    let thumb: HtmlElement = document.create_element("div")?.dyn_into()?;
    thumb.style().set_css_text("position:absolute;left:1px;right:1px;top:0;border-radius:3px;background:var(--app-scrollbar-thumb,rgba(128,128,128,.45))");
    scrollbar.append_child(&thumb)?;
    host.replace_children_with_node_3(&canvas, &input, &scrollbar);
    let mut mount_guard = MountGuard {
        host: host.clone(),
        nodes: vec![
            canvas.clone().into(),
            input.clone().into(),
            scrollbar.clone().into(),
        ],
        committed: false,
    };
    let context: CanvasRenderingContext2d = canvas
        .get_context("2d")?
        .ok_or_else(|| error("Terminal Canvas2D unavailable"))?
        .dyn_into()?;
    context.set_fill_style_str(&options.theme.background.css());
    context.fill_rect(0.0, 0.0, canvas.width() as f64, canvas.height() as f64);
    let font_size = if options.font_size.is_finite() {
        options.font_size.round().clamp(6.0, 32.0)
    } else {
        12.0
    };
    context.set_font(&format!("normal 400 {font_size}px {DEFAULT_FONT}"));
    // Symbols-only font composes with installed text/Nerd fonts without
    // changing their advance widths. The bytes are the original bundled face.
    if let Ok(face) = web_sys::FontFace::new_with_u8_array(
        "Symbols Nerd Font Mono",
        include_bytes!("../fonts/SymbolsNerdFontMono-Regular.woff2"),
    ) {
        if let Ok(promise) = face.load() {
            if wasm_bindgen_futures::JsFuture::from(promise).await.is_ok() {
                let _ = document.fonts().add(&face);
            }
        }
    }
    // Wait for actual text faces before fitting, as in the original surface.
    for variant in ["normal 400", "normal 700", "italic 400", "italic 700"] {
        let promise = document.fonts().load_with_text(
            &format!("{variant} {font_size}px {DEFAULT_FONT}"),
            "iMW0@# .",
        );
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
    let width = context.measure_text("M")?.width();
    let vertical = context.measure_text("Mg")?;
    let metrics = renderer::measured_metrics(
        font_size,
        width,
        vertical.actual_bounding_box_ascent(),
        vertical.actual_bounding_box_descent(),
    );
    let platform = web_sys::window()
        .and_then(|w| w.navigator().platform().ok())
        .unwrap_or_default();
    let layout_map = load_layout().await;
    let core = TerminalCore::create(1, 1, &options.theme).await?;
    let outbox = Rc::new(RefCell::new(vec![]));
    let writable = Rc::new(Cell::new(!options.read_only));
    let state = Rc::new(RefCell::new(State {
        core,
        host: host.clone(),
        canvas: canvas.clone(),
        input: input.clone(),
        context,
        scrollbar: scrollbar.clone(),
        thumb,
        scroll_pointer: None,
        scroll_offset: 0.0,
        last_origin: 4.0,
        callback,
        outbox: outbox.clone(),
        theme: options.theme,
        metrics,
        font_size,
        visible: true,
        read_only: options.read_only,
        writable: writable.clone(),
        disposed: false,
        focused: false,
        full: true,
        cursor_on: true,
        frame: None,
        blink: None,
        resize_timer: None,
        composition_timer: None,
        frame_callback: None,
        blink_callback: None,
        resize_callback: None,
        composition_callback: None,
        snapshot: None,
        previous_cursor: None,
        grid: (0, 0),
        mount_height: 0.0,
        composing: false,
        composition_suppress: None,
        suppressed: BTreeSet::new(),
        paste_token: 0,
        copy_token: 0,
        copy_clear: false,
        mac: platform.to_lowercase().contains("mac")
            || platform.starts_with("iPhone")
            || platform.starts_with("iPad"),
        wheel_remainder: 0.0,
        layout_map,
        anchor: None,
        selection_base: None,
        selection_mode: 1,
        selection_end: None,
        selection_moved: false,
        clicks: None,
        selection_scroll: 0,
        selection_timer: None,
        selection_pointer: None,
        selection_callback: None,
        pointer: None,
        mouse_button: None,
        last_mouse: String::new(),
        mouse_tracking: false,
    }));
    // PTY replies are queued until the current Rust operation releases State.
    let pty_out = outbox;
    let writer = Closure::wrap(Box::new(move |data: String| {
        if writable.get() {
            pty_out
                .borrow_mut()
                .push(json!({"type":"write","data":data}));
        }
    }) as Box<dyn Fn(String)>);
    state
        .borrow_mut()
        .core
        .set_writer(writer.as_ref().unchecked_ref::<Function>().clone())?;
    let weak = Rc::downgrade(&state);
    state.borrow_mut().selection_callback = Some(Closure::wrap(Box::new(move || {
        if let Some(state) = weak.upgrade() {
            {
                let mut s = state.borrow_mut();
                if s.disposed || s.selection_scroll == 0 {
                    return;
                }
                let rows = s.selection_scroll;
                let result = (|| {
                    s.core.scroll(rows)?;
                    if let Some((x, y)) = s.selection_pointer {
                        let point = s.point_at(x, y);
                        s.extend_selection(point)?;
                    }
                    Ok::<(), JsValue>(())
                })();
                if let Err(cause) = result {
                    s.fail(cause)
                }
                s.full = true;
                s.request_frame();
            }
            dispatch(&state);
        }
    }) as Box<dyn Fn()>));
    // Core owns the JS Function; keep its Rust closure alive for this surface.
    let weak = Rc::downgrade(&state);
    let frame = Closure::wrap(Box::new(move |_time: f64| {
        if let Some(state) = weak.upgrade() {
            {
                let mut state = state.borrow_mut();
                state.frame = None;
                if let Err(cause) = state.draw() {
                    state.fail(cause)
                }
            }
            dispatch(&state);
        }
    }) as Box<dyn Fn(f64)>);
    state.borrow_mut().frame_callback = Some(frame);
    state.borrow_mut().blink_callback = Some(timer(&state, |s| {
        s.blink = None;
        s.cursor_on = !s.cursor_on;
        s.request_frame();
        Ok(())
    }));
    state.borrow_mut().resize_callback = Some(timer(&state, |s| {
        s.resize_timer = None;
        let (cols, rows) = s.grid;
        s.queue(json!({"type":"resize","cols":cols,"rows":rows}));
        Ok(())
    }));
    state.borrow_mut().composition_callback = Some(timer(&state, |s| {
        s.composition_timer = None;
        s.composition_suppress = None;
        Ok(())
    }));
    let mut listeners = vec![];
    let textarea: EventTarget = input.clone().into();
    let canvas_target: EventTarget = canvas.clone().into();
    for release in [false, true] {
        let clipboard_owner = Rc::downgrade(&state);
        listeners.push(listen(
            &state,
            textarea.clone(),
            if release { "keyup" } else { "keydown" },
            move |s, event| {
                let event: KeyboardEvent = event.dyn_into()?;
                if release && s.suppressed.remove(&event.code()) {
                    return Ok(());
                }
                if event.get_modifier_state("AltGraph") && event.key().chars().count() == 1 {
                    s.suppressed.insert(event.code());
                    return Ok(());
                }
                if s.composing
                    || event.is_composing()
                    || event.key_code() == 229
                    || event.key() == "Process"
                {
                    return Ok(());
                }
                let control = event.ctrl_key() || event.meta_key();
                let key = event.key().to_lowercase();
                let copy = if key == "insert" && !s.mac {
                    event.ctrl_key() && !event.shift_key() && !event.meta_key()
                } else if key == "c" {
                    if s.mac {
                        event.meta_key()
                    } else {
                        event.ctrl_key()
                    }
                } else {
                    false
                };
                if !release && copy {
                    let selection = s.core.selection_text()?;
                    if !selection.is_empty() {
                        s.input.set_value(&selection);
                        s.input.select();
                        s.copy_clear = !event.shift_key() && !s.mac && key != "insert";
                        s.copy_token += 1;
                        let token = s.copy_token;
                        if let Some(window) = web_sys::window() {
                            let clipboard = window.navigator().clipboard();
                            let owner = clipboard_owner.clone();
                            let text = selection.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                // Native default copy runs first and claims the token.
                                let _ = wasm_bindgen_futures::JsFuture::from(
                                    js_sys::Promise::resolve(&JsValue::UNDEFINED),
                                )
                                .await;
                                let current = owner.upgrade().is_some_and(|s| {
                                    let s = s.borrow();
                                    !s.disposed && s.copy_token == token
                                });
                                if !current {
                                    return;
                                }
                                let result = wasm_bindgen_futures::JsFuture::from(
                                    clipboard.write_text(&text),
                                )
                                .await;
                                if let Some(state) = owner.upgrade() {
                                    let mut s = state.borrow_mut();
                                    if s.disposed || s.copy_token != token {
                                        return;
                                    }
                                    if result.is_ok() && s.copy_clear {
                                        let _ = s.core.clear_selection();
                                        s.input.set_value("");
                                        s.request_frame();
                                    }
                                    s.copy_clear = false;
                                }
                            });
                        }
                        if event.shift_key() || key == "insert" {
                            event.prevent_default();
                        }
                        s.suppressed.insert(event.code());
                        return Ok(());
                    }
                }
                let paste = if key == "insert" && !s.mac {
                    event.shift_key() && !event.ctrl_key() && !event.meta_key()
                } else if key == "v" {
                    if s.mac {
                        event.meta_key()
                    } else {
                        event.ctrl_key() && event.shift_key()
                    }
                } else {
                    false
                };
                if !release && paste {
                    s.suppressed.insert(event.code());
                    s.paste_token += 1;
                    let token = s.paste_token;
                    let owner = clipboard_owner.clone();
                    if let Some(window) = web_sys::window() {
                        let clipboard = window.navigator().clipboard();
                        wasm_bindgen_futures::spawn_local(async move {
                            if let Ok(value) =
                                wasm_bindgen_futures::JsFuture::from(clipboard.read_text()).await
                            {
                                if let Some(state) = owner.upgrade() {
                                    {
                                        let mut s = state.borrow_mut();
                                        if s.disposed || s.paste_token != token {
                                            return;
                                        }
                                        s.paste_token += 1;
                                        if let Some(text) = value.as_string() {
                                            match s.core.encode_paste(&text) {
                                                Ok(data) => s.send(data),
                                                Err(cause) => s.fail(cause),
                                            }
                                        }
                                    }
                                    dispatch(&state);
                                }
                            }
                        });
                    }
                    return Ok(());
                }
                if s.read_only {
                    return Ok(());
                }
                if !release && control && key == "a" && event.shift_key() {
                    s.core.select_all()?;
                    event.prevent_default();
                    s.suppressed.insert(event.code());
                    return Ok(());
                }
                let input = KeyInput {
                    code: event.code(),
                    key: event.key(),
                    shift_key: event.shift_key(),
                    ctrl_key: event.ctrl_key(),
                    alt_key: event.alt_key(),
                    meta_key: event.meta_key(),
                    caps_lock: event.get_modifier_state("CapsLock"),
                    num_lock: event.get_modifier_state("NumLock"),
                    repeat: event.repeat(),
                    is_composing: event.is_composing(),
                    release,
                    layout_character: s.layout_map.as_ref().and_then(|map| {
                        let get: Function = js_sys::Reflect::get(map, &"get".into())
                            .ok()?
                            .dyn_into()
                            .ok()?;
                        get.call1(map, &event.code().into()).ok()?.as_string()
                    }),
                };
                let data = s.core.encode_key(&serde_json::to_string(&input).unwrap())?;
                if !data.is_empty() {
                    event.prevent_default();
                    event.stop_propagation();
                    s.input.set_value("");
                    s.send(data)
                }
                Ok(())
            },
        )?);
    }
    listeners.push(listen(&state, textarea.clone(), "paste", |s, event| {
        let event: ClipboardEvent = event.dyn_into()?;
        event.prevent_default();
        let text = event
            .clipboard_data()
            .and_then(|data| data.get_data("text/plain").ok())
            .unwrap_or_default();
        if !text.is_empty() {
            s.paste_token += 1;
            let data = s.core.encode_paste(&text)?;
            s.send(data)
        }
        Ok(())
    })?);
    listeners.push(listen(&state, textarea.clone(), "copy", |s, event| {
        let event: ClipboardEvent = event.dyn_into()?;
        let text = s.core.selection_text()?;
        if !text.is_empty() {
            if let Some(clipboard) = event.clipboard_data() {
                clipboard.set_data("text/plain", &text)?;
                event.prevent_default();
                s.copy_token += 1;
                if s.copy_clear {
                    s.copy_clear = false;
                    s.core.clear_selection()?;
                    s.input.set_value("");
                }
            }
        }
        Ok(())
    })?);
    listeners.push(listen(
        &state,
        textarea.clone(),
        "compositionstart",
        |s, _| {
            s.clear_composition();
            s.input.set_value("");
            s.composing = true;
            Ok(())
        },
    )?);
    listeners.push(listen(
        &state,
        textarea.clone(),
        "compositionend",
        |s, event| {
            let event: CompositionEvent = event.dyn_into()?;
            s.composing = false;
            let text = if !s.input.value().is_empty() {
                s.input.value()
            } else {
                event.data().unwrap_or_default()
            };
            s.send(text.clone());
            s.input.set_value("");
            s.composition_suppress = Some(text);
            if let (Some(window), Some(callback)) =
                (web_sys::window(), s.composition_callback.as_ref())
            {
                s.composition_timer = window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        callback.as_ref().unchecked_ref(),
                        100,
                    )
                    .ok();
            }
            Ok(())
        },
    )?);
    listeners.push(listen(&state, textarea.clone(), "input", |s, event| {
        let event: InputEvent = event.dyn_into()?;
        if s.composing || event.is_composing() {
            return Ok(());
        }
        let text = if !s.input.value().is_empty() {
            s.input.value()
        } else {
            event.data().unwrap_or_default()
        };
        let suppressed = s.composition_suppress.as_ref() == Some(&text)
            && matches!(
                event.input_type().as_str(),
                "" | "insertCompositionText" | "insertFromComposition"
            );
        s.clear_composition();
        s.input.set_value("");
        if !suppressed {
            s.send(text)
        }
        Ok(())
    })?);
    listeners.push(listen(&state, textarea.clone(), "focus", |s, _| {
        s.focused = true;
        s.cursor_on = true;
        s.queue(json!({"type":"focus"}));
        Ok(())
    })?);
    listeners.push(listen(&state, textarea.clone(), "blur", |s, _| {
        s.focused = false;
        s.cursor_on = true;
        s.composing = false;
        Ok(())
    })?);
    listeners.push(listen(
        &state,
        canvas_target.clone(),
        "pointerdown",
        |s, event| {
            let event: PointerEvent = event.dyn_into()?;
            s.queue(json!({"type":"__focus"}));
            if s.core.is_mouse_tracking()?
                && !event.shift_key()
                && !event.ctrl_key()
                && !event.meta_key()
            {
                s.mouse_button = match event.button() {
                    0 => Some(1),
                    1 => Some(3),
                    2 => Some(2),
                    3 => Some(4),
                    4 => Some(5),
                    _ => None,
                };
                if s.mouse_button.is_none() {
                    return Ok(());
                }
                s.mouse(&event, "press", s.mouse_button)?;
                s.pointer = Some(event.pointer_id());
                s.canvas.set_pointer_capture(event.pointer_id())?;
                event.prevent_default();
                return Ok(());
            }
            if event.button() != 0 {
                return Ok(());
            }
            event.prevent_default();
            let point = s.point(&event);
            s.selection_pointer = Some((event.client_x() as f64, event.client_y() as f64));
            let screen = s.core.convert_point(point, 2)?.map(|p| Point {
                x: p.x,
                y: p.y,
                tag: 2,
            });
            s.anchor = screen;
            s.selection_moved = false;
            s.clicks = Some(crate::surface_policy::advance_click(
                s.clicks,
                event.time_stamp(),
                event.client_x() as f64,
                event.client_y() as f64,
            ));
            s.selection_mode = s.clicks.unwrap().count;
            let range = if s.selection_mode > 1 {
                s.core.select_at(point.x, point.y, s.selection_mode >= 3)?
            } else {
                None
            };
            s.selection_base = range.map(|range| range.screen);
            if let Some(range) = range {
                s.anchor = Some(Point {
                    x: range.screen.start.x,
                    y: range.screen.start.y,
                    tag: 2,
                });
                s.selection_end = Some(Point {
                    x: range.viewport.end.x,
                    y: range.viewport.end.y,
                    tag: 1,
                });
            } else {
                s.selection_mode = 1;
                s.selection_end = Some(point);
                let at = screen.unwrap_or(point);
                s.core.set_selection(at, at)?;
            }
            s.full = true;
            s.pointer = Some(event.pointer_id());
            s.canvas.set_pointer_capture(event.pointer_id())?;
            Ok(())
        },
    )?);
    listeners.push(listen(
        &state,
        canvas_target.clone(),
        "pointermove",
        |s, event| {
            let event: PointerEvent = event.dyn_into()?;
            s.synchronize_mouse()?;
            if (s.mouse_button.is_some() && s.pointer == Some(event.pointer_id()))
                || (s.mouse_tracking
                    && !event.shift_key()
                    && !event.ctrl_key()
                    && !event.meta_key())
            {
                s.mouse(&event, "motion", s.mouse_button)?;
                return Ok(());
            }
            s.last_mouse.clear();
            if s.pointer != Some(event.pointer_id()) {
                return Ok(());
            }
            let bounds = s.canvas.get_bounding_client_rect();
            s.selection_pointer = Some((event.client_x() as f64, event.client_y() as f64));
            s.selection_autoscroll(if (event.client_y() as f64) < bounds.top() {
                -1
            } else if (event.client_y() as f64) > bounds.bottom() {
                1
            } else {
                0
            });
            let point = s.point(&event);
            if !s
                .selection_end
                .is_some_and(|last| last.x == point.x && last.y == point.y)
            {
                s.extend_selection(point)?;
            }
            Ok(())
        },
    )?);
    for name in ["pointerup", "pointercancel"] {
        listeners.push(listen(&state, canvas_target.clone(), name, |s, event| {
            let event: PointerEvent = event.dyn_into()?;
            s.selection_autoscroll(0);
            if s.pointer == Some(event.pointer_id()) {
                if s.mouse_button.is_some() {
                    s.mouse(&event, "release", s.mouse_button)?;
                }
                if s.mouse_button.is_none() && !s.selection_moved && s.selection_mode == 1 {
                    s.core.clear_selection()?;
                    s.full = true;
                }
                s.pointer = None;
                s.mouse_button = None;
                if s.canvas.has_pointer_capture(event.pointer_id()) {
                    s.canvas.release_pointer_capture(event.pointer_id())?;
                }
            }
            let selection = s.core.selection_text()?;
            s.input.set_value(&selection);
            if !selection.is_empty() {
                s.input.select()
            }
            Ok(())
        })?);
    }
    listeners.push(listen(
        &state,
        canvas_target.clone(),
        "wheel",
        |s, event| {
            let event: WheelEvent = event.dyn_into()?;
            event.prevent_default();
            let delta=crate::surface_policy::wheel(event.delta_y(),event.delta_mode(),s.metrics.height,s.grid.1,s.wheel_remainder);
            s.wheel_remainder=delta.remainder;let rows=delta.rows;
            if rows != 0 {
                if s.core.is_mouse_tracking()?&&!event.shift_key()&&!event.ctrl_key()&&!event.meta_key(){
                    let bounds=s.canvas.get_bounding_client_rect();
                    for _ in 0..rows.unsigned_abs(){
                        let data=s.core.encode_mouse(&json!({"action":"press","button":if rows<0{4}else{5},"mods":u32::from(event.shift_key())|u32::from(event.ctrl_key())<<1|u32::from(event.alt_key())<<2|u32::from(event.meta_key())<<3,
                          "x":event.client_x() as f64-bounds.left(),"y":event.client_y() as f64-bounds.top(),"screenWidth":bounds.width(),"screenHeight":bounds.height(),"cellWidth":s.metrics.width,"cellHeight":s.metrics.height,"paddingLeft":4,"paddingRight":4,"paddingTop":4,"paddingBottom":4,"anyButtonPressed":false}).to_string())?;
                        s.send(data);
                    }
                }else if s.core.is_alternate_screen()?{
                    let data=crate::surface_policy::wheel_arrows(rows,s.core.mode(1)?);s.send(data);
                }else{s.core.scroll(rows)?;s.full=true;}
            }
            Ok(())
        },
    )?);
    let scroll_target: EventTarget = scrollbar.clone().into();
    listeners.push(listen(
        &state,
        scroll_target.clone(),
        "pointerdown",
        |s, event| {
            let event: PointerEvent = event.dyn_into()?;
            event.prevent_default();
            let bounds = s.thumb.get_bounding_client_rect();
            s.scroll_offset = if event.target() == Some(s.thumb.clone().into()) {
                event.client_y() as f64 - bounds.top()
            } else {
                bounds.height() / 2.0
            };
            s.scroll_pointer = Some(event.pointer_id());
            s.scrollbar.set_pointer_capture(event.pointer_id())?;
            s.scroll_pointer_to(event.client_y() as f64)
        },
    )?);
    listeners.push(listen(
        &state,
        scroll_target.clone(),
        "pointermove",
        |s, event| {
            let event: PointerEvent = event.dyn_into()?;
            if s.scroll_pointer == Some(event.pointer_id()) {
                event.prevent_default();
                s.scroll_pointer_to(event.client_y() as f64)?;
            }
            Ok(())
        },
    )?);
    for name in ["pointerup", "pointercancel"] {
        listeners.push(listen(&state, scroll_target.clone(), name, |s, event| {
            let event: PointerEvent = event.dyn_into()?;
            if s.scroll_pointer == Some(event.pointer_id()) {
                s.scroll_pointer = None;
                if s.scrollbar.has_pointer_capture(event.pointer_id()) {
                    s.scrollbar.release_pointer_capture(event.pointer_id())?;
                }
            }
            Ok(())
        })?);
    }
    listeners.push(listen(&state, scroll_target, "keydown", |s, event| {
        let event: KeyboardEvent = event.dyn_into()?;
        let rows = match event.key().as_str() {
            "ArrowUp" => -1,
            "ArrowDown" => 1,
            "PageUp" => -(s.grid.1 as i32),
            "PageDown" => s.grid.1 as i32,
            "Home" => -10000,
            "End" => 10000,
            _ => return Ok(()),
        };
        event.prevent_default();
        s.core.scroll(rows)?;
        s.full = true;
        Ok(())
    })?);
    let weak = Rc::downgrade(&state);
    let resize = Closure::wrap(Box::new(move || {
        if let Some(state) = weak.upgrade() {
            {
                let mut state = state.borrow_mut();
                if let Err(cause) = state.fit() {
                    state.fail(cause)
                }
            }
            dispatch(&state);
        }
    }) as Box<dyn Fn()>);
    let observer = ResizeObserver::new(resize.as_ref().unchecked_ref())?;
    observer.observe(&host);
    let surface = TerminalSurface {
        state,
        listeners,
        observer,
        _resize: resize,
        _writer: writer,
    };
    // Own observers/listeners/timers before the last fallible initialization:
    // Drop disconnects everything if fitting or first paint fails.
    surface.state.borrow_mut().fit()?;
    dispatch(&surface.state);
    mount_guard.committed = true;
    Ok(surface)
}
#[wasm_bindgen]
impl TerminalSurface {
    pub fn write(&self, data: &str) -> Result<()> {
        {
            let mut s = self.state.borrow_mut();
            s.core.write(data)?;
            s.synchronize_mouse()?;
            s.cursor_on = true;
            s.request_frame();
        }
        dispatch(&self.state);
        Ok(())
    }
    pub fn reset_and_write(&self, data: &str) -> Result<()> {
        {
            let mut s = self.state.borrow_mut();
            s.core.reset_and_write(data)?;
            s.last_mouse.clear();
            s.synchronize_mouse()?;
            s.full = true;
            s.cursor_on = true;
            s.request_frame();
        }
        dispatch(&self.state);
        Ok(())
    }
    pub fn set_visible(&self, visible: bool) -> Result<()> {
        let mut s = self.state.borrow_mut();
        s.visible = visible;
        s.cursor_on = true;
        s.full = true;
        if visible {
            s.fit()?
        } else {
            s.cancel_frame();
            s.selection_autoscroll(0);
        }
        Ok(())
    }
    pub fn set_read_only(&self, read_only: bool) {
        let mut state = self.state.borrow_mut();
        state.read_only = read_only;
        state.writable.set(!read_only);
    }
    pub fn focus(&self) -> Result<()> {
        let input = self.state.borrow().input.clone();
        input.focus()
    }
    pub fn fit(&self) -> Result<()> {
        {
            self.state.borrow_mut().fit()?;
        }
        dispatch(&self.state);
        Ok(())
    }
    pub fn resend_size(&self) {
        let s = self.state.borrow();
        let (cols, rows) = s.grid;
        s.queue(json!({"type":"resize","cols":cols,"rows":rows}));
        drop(s);
        dispatch(&self.state)
    }
    pub fn clear_selection(&self) -> Result<()> {
        let mut s = self.state.borrow_mut();
        s.core.clear_selection()?;
        s.input.set_value("");
        s.request_frame();
        Ok(())
    }
    pub fn selection_text(&self) -> Result<String> {
        self.state.borrow().core.selection_text()
    }
    pub fn dispose(&mut self) {
        self.cleanup();
    }
}
impl TerminalSurface {
    fn cleanup(&mut self) {
        self.observer.disconnect();
        self.listeners.clear();
        {
            let mut s = self.state.borrow_mut();
            if s.disposed {
                return;
            }
            s.disposed = true;
            s.writable.set(false);
            s.cancel_frame();
            s.selection_autoscroll(0);
            s.clear_composition();
            if let (Some(window), Some(id)) = (web_sys::window(), s.resize_timer.take()) {
                window.clear_timeout_with_handle(id);
                let (cols, rows) = s.grid;
                s.queue(json!({"type":"resize","cols":cols,"rows":rows}));
            }
            s.canvas.remove();
            s.input.remove();
            s.scrollbar.remove();
        }
        dispatch(&self.state);
    }
}
impl Drop for TerminalSurface {
    fn drop(&mut self) {
        self.cleanup()
    }
}

async fn load_layout() -> Option<JsValue> {
    let navigator = web_sys::window()?.navigator();
    let keyboard = js_sys::Reflect::get(&navigator, &"keyboard".into()).ok()?;
    let function: Function = js_sys::Reflect::get(&keyboard, &"getLayoutMap".into())
        .ok()?
        .dyn_into()
        .ok()?;
    let promise: js_sys::Promise = function.call0(&keyboard).ok()?.dyn_into().ok()?;
    wasm_bindgen_futures::JsFuture::from(promise).await.ok()
}
