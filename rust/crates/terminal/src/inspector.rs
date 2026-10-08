//! Synchronous theme dependency probing. Sentinel values are restored before paint.
use crate::runtime::{Result, error};
use std::collections::BTreeMap;
use t3_client::themes::inspector::{self as policy, Bounds, Paint, PaintKind, Rectangle};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{CssStyleDeclaration, Document, Element, HtmlElement, Node};
const MATCH: &str = "data-theme-inspector-match";
const PROBE: &str = "data-theme-token-probe";
const SPOTLIGHT: &str = "theme-inspector-spotlight";
const HOVER: &str = "theme-inspector-hover";
const EDITOR: &str = "[data-theme-editor-panel]";
const SVG: &str = "http://www.w3.org/2000/svg";
fn document() -> Result<Document> {
    web_sys::window()
        .and_then(|window| window.document())
        .ok_or_else(|| error("Theme inspector document is unavailable"))
}
fn root() -> Result<HtmlElement> {
    document()?
        .document_element()
        .ok_or_else(|| error("Theme inspector root is unavailable"))?
        .dyn_into()
        .map_err(|_| error("Theme inspector root is not HTML"))
}
fn computed(element: &Element) -> Result<CssStyleDeclaration> {
    web_sys::window()
        .ok_or_else(|| error("Theme inspector window is unavailable"))?
        .get_computed_style(element)?
        .ok_or_else(|| error("Theme inspector style is unavailable"))
}
fn query(document: &Document, selector: &str) -> Result<Vec<Element>> {
    let nodes = document.query_selector_all(selector)?;
    Ok((0..nodes.length())
        .filter_map(|index| nodes.item(index)?.dyn_into().ok())
        .collect())
}
fn excluded(element: &Element) -> Result<bool> {
    Ok(element.closest(EDITOR)?.is_some()
        || element
            .closest(&format!("#{SPOTLIGHT}, #{HOVER}"))?
            .is_some())
}
fn candidates(initial: &Element) -> Result<Vec<Element>> {
    let root = document()?.document_element();
    let mut node = Some(initial.clone());
    let mut output = Vec::new();
    while let Some(element) = node {
        if root.as_ref() == Some(&element) {
            break;
        }
        if element.closest(EDITOR)?.is_none() {
            output.push(element.clone());
        }
        node = element.parent_element();
    }
    Ok(output)
}
fn visible_text(element: &Element) -> Result<bool> {
    if element.matches("input, textarea, select, option")? {
        return Ok(true);
    }
    let nodes = element.child_nodes();
    Ok((0..nodes.length()).any(|index| {
        nodes.item(index).is_some_and(|node| {
            node.node_type() == Node::TEXT_NODE
                && node
                    .text_content()
                    .is_some_and(|text| !js_sys::JsString::from(text).trim().length().eq(&0))
        })
    }))
}
fn paint(element: &Element, hit_test: bool) -> Result<Option<Paint>> {
    let style = computed(element)?;
    let property = |name: &str| style.get_property_value(name);
    if property("display")? == "none"
        || property("visibility")? == "hidden"
        || property("opacity")?
            .parse::<js_sys::Number>()
            .unwrap()
            .value_of()
            == 0.
    {
        return Ok(None);
    }
    let mut border = Vec::new();
    for side in ["top", "right", "bottom", "left"] {
        if property(&format!("border-{side}-style"))? != "none"
            && js_sys::parse_float(&property(&format!("border-{side}-width"))?) > 0.
        {
            border.push(property(&format!("border-{side}-color"))?);
        }
    }
    if property("outline-style")? != "none" && js_sys::parse_float(&property("outline-width")?) > 0.
    {
        border.push(property("outline-color")?);
    }
    for key in ["border-image-source", "box-shadow"] {
        let value = property(key)?;
        if value != "none" {
            border.push(value);
        }
    }
    let mut foreground = Vec::new();
    if hit_test || visible_text(element)? {
        foreground.push(property("color")?);
    }
    if element.dyn_ref::<web_sys::SvgElement>().is_some() {
        foreground.push(property("fill")?);
        foreground.push(property("stroke")?);
    }
    if element.matches("input, textarea")? {
        foreground.push(property("caret-color")?);
    }
    if property("text-decoration-line")? != "none" {
        foreground.push(property("text-decoration-color")?);
    }
    let shadow = property("text-shadow")?;
    if shadow != "none" {
        foreground.push(shadow);
    }
    Ok(Some(Paint {
        background: format!(
            "{}\n{}",
            property("background-color")?,
            property("background-image")?
        ),
        border: border.join("\n"),
        foreground: foreground.join("\n"),
    }))
}
struct ProbeSession {
    root: HtmlElement,
    owns: bool,
}
impl ProbeSession {
    fn begin() -> Result<Self> {
        let root = root()?;
        let owns = !root.has_attribute(PROBE);
        if owns {
            root.set_attribute(PROBE, "")?;
        }
        Ok(Self { root, owns })
    }
}
impl Drop for ProbeSession {
    fn drop(&mut self) {
        let _ = computed(&self.root).and_then(|style| style.get_property_value("color"));
        if self.owns {
            let _ = self.root.remove_attribute(PROBE);
        }
    }
}
struct TokenProbe {
    style: CssStyleDeclaration,
    variable: String,
    value: String,
    priority: String,
}
impl TokenProbe {
    fn begin(variable: &str) -> Result<Self> {
        let style = root()?.style();
        let value = style.get_property_value(variable)?;
        let priority = style.get_property_priority(variable);
        let sentinel = if js_sys::JsString::from(value.as_str())
            .trim()
            .to_lower_case()
            .as_string()
            .as_deref()
            == Some("#01fea7")
        {
            "#fe01a7"
        } else {
            "#01fea7"
        };
        // Construct ownership before mutation: even an exception unwinds restoration.
        let probe = Self {
            style,
            variable: variable.into(),
            value,
            priority,
        };
        probe
            .style
            .set_property_with_priority(variable, sentinel, "important")?;
        Ok(probe)
    }
}
impl Drop for TokenProbe {
    fn drop(&mut self) {
        if self.value.is_empty() {
            let _ = self.style.remove_property(&self.variable);
        } else {
            let _ =
                self.style
                    .set_property_with_priority(&self.variable, &self.value, &self.priority);
        }
    }
}
struct TokenProbes(Vec<TokenProbe>);
impl Drop for TokenProbes {
    fn drop(&mut self) {
        while let Some(probe) = self.0.pop() {
            drop(probe);
        }
    }
}
fn utilities(initial: &Element) -> Result<Option<(Element, String)>> {
    if initial.closest(EDITOR)?.is_some() {
        return Ok(None);
    }
    for element in candidates(initial)? {
        let classes = element.class_list();
        for kind in PaintKind::ORDER {
            for index in 0..classes.length() {
                if let Some(role) = classes
                    .item(index)
                    .and_then(|class| policy::utility(&class, kind))
                {
                    return Ok(Some((element, role.into())));
                }
            }
        }
    }
    Ok(None)
}
pub fn inspect(
    initial: &Element,
    roles: &[String],
    variables: &BTreeMap<String, String>,
) -> Result<Option<(Element, String)>> {
    if initial.closest(EDITOR)?.is_some() {
        return Ok(None);
    }
    if let Some(found) = utilities(initial)? {
        return Ok(Some(found));
    }
    let _session = ProbeSession::begin()?;
    let mut baseline = Vec::new();
    for element in candidates(initial)? {
        if let Some(before) = paint(&element, true)? {
            baseline.push((element, before, BTreeMap::<usize, String>::new()));
        }
    }
    for role in roles {
        let Some(variable) = variables.get(role) else {
            continue;
        };
        let _probe = TokenProbe::begin(variable)?;
        for (element, before, matched) in &mut baseline {
            if let Some(after) = paint(element, true)? {
                for kind in policy::changed(before, &after) {
                    matched.entry(kind as usize).or_insert_with(|| role.clone());
                }
            }
        }
    }
    for (element, _, matched) in baseline {
        if let Some((_, role)) = matched.into_iter().next() {
            return Ok(Some((element, role)));
        }
    }
    Ok(None)
}
pub fn clear_highlights() -> Result<()> {
    let document = document()?;
    for element in query(&document, &format!("[{MATCH}]"))? {
        element.remove_attribute(MATCH)?;
    }
    if let Some(element) = document.get_element_by_id(SPOTLIGHT) {
        element.remove();
    }
    Ok(())
}
pub fn clear_hover() -> Result<()> {
    if let Some(element) = document()?.get_element_by_id(HOVER) {
        element.remove();
    }
    Ok(())
}
pub fn highlight(roles: &[String], variables: &BTreeMap<String, String>) -> Result<usize> {
    let document = document()?;
    for element in query(&document, &format!("[{MATCH}]"))? {
        element.remove_attribute(MATCH)?;
    }
    let mut baseline = Vec::new();
    if let Some(body) = document.body() {
        let mut candidates = vec![body.into()];
        candidates.extend(query(&document, "body *")?);
        for element in candidates {
            if !excluded(&element)? {
                if let Some(before) = paint(&element, false)? {
                    baseline.push((element, before));
                }
            }
        }
    }
    let mut changed = Vec::new();
    {
        let _session = ProbeSession::begin()?;
        let mut probes = TokenProbes(Vec::new());
        for role in roles {
            if let Some(variable) = variables.get(role) {
                probes.0.push(TokenProbe::begin(variable)?);
            }
        }
        for (element, before) in baseline {
            if let Some(after) = paint(&element, false)? {
                if !policy::changed(&before, &after).is_empty() {
                    let normalized = if element.dyn_ref::<web_sys::SvgElement>().is_some() {
                        element.closest("svg")?.unwrap_or(element)
                    } else {
                        element
                    };
                    if !changed.contains(&normalized) {
                        changed.push(normalized);
                    }
                }
            }
        }
        // Reverse restoration matches overlapping/duplicate probes in the source.
        drop(probes);
    }
    for element in &changed {
        element.set_attribute(MATCH, "")?;
    }
    render_spotlight(&changed)?;
    Ok(changed.len())
}
fn rectangle(element: &Element) -> Result<Option<Rectangle>> {
    let window = web_sys::window().ok_or_else(|| error("Theme inspector window is unavailable"))?;
    let bounds = element.get_bounding_client_rect();
    let radius =
        js_sys::parse_float(&computed(element)?.get_property_value("border-top-left-radius")?);
    let radius = if radius.is_nan() || radius == 0. {
        0.
    } else {
        radius
    };
    Ok(policy::rectangle(
        Bounds {
            left: bounds.left(),
            top: bounds.top(),
            width: bounds.width(),
            height: bounds.height(),
            right: bounds.right(),
            bottom: bounds.bottom(),
        },
        [
            window.inner_width()?.as_f64().unwrap_or(0.),
            window.inner_height()?.as_f64().unwrap_or(0.),
        ],
        radius,
    ))
}
fn svg(document: &Document, name: &str, attributes: &[(&str, String)]) -> Result<Element> {
    let element = document.create_element_ns(Some(SVG), name)?;
    for (key, value) in attributes {
        element.set_attribute(key, value)?;
    }
    Ok(element)
}
fn append(parent: &Element, child: &Element) -> Result<()> {
    parent.append_child(child).map(|_| ())
}
pub fn render_spotlight(elements: &[Element]) -> Result<()> {
    let document = document()?;
    let mut rectangles = Vec::<(String, Rectangle)>::new();
    for element in elements {
        if let Some(rectangle) = rectangle(element)? {
            let key = [rectangle.x, rectangle.y, rectangle.width, rectangle.height]
                .map(|n| js_sys::Math::round(n).to_string())
                .join(":");
            if let Some((_, value)) = rectangles.iter_mut().find(|(existing, _)| *existing == key) {
                *value = rectangle;
            } else {
                rectangles.push((key, rectangle));
            }
        }
    }
    if rectangles.is_empty() {
        if let Some(element) = document.get_element_by_id(SPOTLIGHT) {
            element.remove();
        }
        return Ok(());
    }
    let window = web_sys::window().unwrap();
    let width = window.inner_width()?.as_f64().unwrap_or(0.).to_string();
    let height = window.inner_height()?.as_f64().unwrap_or(0.).to_string();
    let spotlight = match document.get_element_by_id(SPOTLIGHT) {
        Some(element) => element,
        None => {
            let element = svg(
                &document,
                "svg",
                &[
                    ("id", SPOTLIGHT.into()),
                    ("aria-hidden", "true".into()),
                    ("focusable", "false".into()),
                ],
            )?;
            document
                .body()
                .ok_or_else(|| error("Inspector body is unavailable"))?
                .append_child(&element)?;
            element
        }
    };
    spotlight.set_attribute("viewBox", &format!("0 0 {width} {height}"))?;
    let defs = svg(&document, "defs", &[])?;
    let mask = svg(
        &document,
        "mask",
        &[
            ("id", "theme-inspector-spotlight-mask".into()),
            ("maskUnits", "userSpaceOnUse".into()),
        ],
    )?;
    append(
        &mask,
        &svg(
            &document,
            "rect",
            &[
                ("width", width.clone()),
                ("height", height.clone()),
                ("fill", "white".into()),
            ],
        )?,
    )?;
    let filter = svg(
        &document,
        "filter",
        &[
            ("id", "theme-inspector-spotlight-glow".into()),
            ("x", "-50%".into()),
            ("y", "-50%".into()),
            ("width", "200%".into()),
            ("height", "200%".into()),
        ],
    )?;
    append(
        &filter,
        &svg(
            &document,
            "feGaussianBlur",
            &[("stdDeviation", "5".into()), ("result", "blur".into())],
        )?,
    )?;
    let merge = svg(&document, "feMerge", &[])?;
    append(
        &merge,
        &svg(&document, "feMergeNode", &[("in", "blur".into())])?,
    )?;
    append(
        &merge,
        &svg(&document, "feMergeNode", &[("in", "SourceGraphic".into())])?,
    )?;
    append(&filter, &merge)?;
    append(&defs, &mask)?;
    append(&defs, &filter)?;
    let glow = svg(&document, "g", &[])?;
    for (_, rectangle) in rectangles {
        let attributes = [
            ("x", rectangle.x.to_string()),
            ("y", rectangle.y.to_string()),
            ("width", rectangle.width.to_string()),
            ("height", rectangle.height.to_string()),
            ("rx", rectangle.radius.to_string()),
        ];
        let hole = svg(&document, "rect", &attributes)?;
        hole.set_attribute("fill", "black")?;
        append(&mask, &hole)?;
        let border = svg(&document, "rect", &attributes)?;
        border.set_attribute("class", "theme-inspector-spotlight-glow")?;
        border.set_attribute("filter", "url(#theme-inspector-spotlight-glow)")?;
        append(&glow, &border)?;
    }
    let dimmer = svg(
        &document,
        "rect",
        &[
            ("class", "theme-inspector-spotlight-dimmer".into()),
            ("width", width),
            ("height", height),
            ("mask", "url(#theme-inspector-spotlight-mask)".into()),
        ],
    )?;
    spotlight.set_text_content(None);
    append(&spotlight, &defs)?;
    append(&spotlight, &dimmer)?;
    append(&spotlight, &glow)?;
    Ok(())
}
pub fn refresh_spotlight() -> Result<()> {
    render_spotlight(&query(&document()?, &format!("[{MATCH}]"))?)
}
pub fn hover(element: &Element, label: &str) -> Result<()> {
    let Some(rectangle) = rectangle(element)? else {
        return clear_hover();
    };
    let document = document()?;
    let hover = match document.get_element_by_id(HOVER) {
        Some(element) => element,
        None => {
            let element = document.create_element("div")?;
            element.set_id(HOVER);
            element.set_attribute("aria-hidden", "true")?;
            let label = document.create_element("span")?;
            label.set_attribute("data-theme-inspector-hover-label", "")?;
            append(&element, &label)?;
            document
                .body()
                .ok_or_else(|| error("Inspector body is unavailable"))?
                .append_child(&element)?;
            element
        }
    };
    let html = hover
        .dyn_ref::<HtmlElement>()
        .ok_or_else(|| error("Inspector hover is not HTML"))?;
    for (key, value) in [
        ("left", rectangle.x),
        ("top", rectangle.y),
        ("width", rectangle.width),
        ("height", rectangle.height),
        ("border-radius", rectangle.radius),
    ] {
        html.style().set_property(key, &format!("{value}px"))?;
    }
    hover.set_attribute(
        "data-placement",
        if rectangle.y < 32. { "below" } else { "above" },
    )?;
    if let Some(element) = hover.query_selector("[data-theme-inspector-hover-label]")? {
        element.set_text_content(Some(label));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct Config {
    roles: Vec<String>,
    variables: BTreeMap<String, String>,
}
struct State {
    config: Config,
    callback: js_sys::Function,
    disposed: bool,
    armed: bool,
    clicked: bool,
    roles: Vec<String>,
    target: Option<Element>,
    inspection: Option<(Element, String)>,
    work: Option<js_sys::Function>,
    hover: Option<js_sys::Function>,
    geometry: Option<js_sys::Function>,
    timer: Option<i32>,
    frame: Option<i32>,
    hover_timer: Option<i32>,
    geometry_frame: Option<i32>,
    last_scan: f64,
}
impl State {
    fn window() -> web_sys::Window {
        web_sys::window().expect("mounted inspector has window")
    }
    fn now() -> f64 {
        Self::window().performance().map(|p| p.now()).unwrap_or(0.)
    }
    fn clear_hover(&mut self) -> Result<()> {
        if let Some(timer) = self.hover_timer.take() {
            Self::window().clear_timeout_with_handle(timer);
        }
        self.target = None;
        self.inspection = None;
        clear_hover()
    }
    fn cancel_geometry(&mut self) {
        if let Some(frame) = self.geometry_frame.take() {
            let _ = Self::window().cancel_animation_frame(frame);
        }
    }
    fn cancel_work(&mut self) {
        if let Some(timer) = self.timer.take() {
            Self::window().clear_timeout_with_handle(timer);
        }
        if let Some(frame) = self.frame.take() {
            let _ = Self::window().cancel_animation_frame(frame);
        }
    }
    fn dispose(&mut self) {
        self.disposed = true;
        self.cancel_work();
        self.cancel_geometry();
        let _ = self.clear_hover();
        let _ = clear_highlights();
    }
    fn schedule_work(&mut self) -> Result<()> {
        if self.disposed
            || self.armed
            || self.roles.is_empty()
            || self.timer.is_some()
            || self.frame.is_some()
        {
            return Ok(());
        }
        let remaining = 500. - (Self::now() - self.last_scan);
        if remaining > 0. {
            self.timer = Some(
                Self::window().set_timeout_with_callback_and_timeout_and_arguments_0(
                    self.work.as_ref().unwrap(),
                    remaining.ceil() as i32,
                )?,
            );
        } else {
            self.frame = Some(Self::window().request_animation_frame(self.work.as_ref().unwrap())?);
        }
        Ok(())
    }
    fn schedule_geometry(&mut self) -> Result<()> {
        if !self.disposed
            && self.geometry_frame.is_none()
            && (self.armed && self.inspection.is_some() || !self.armed && !self.roles.is_empty())
        {
            self.geometry_frame =
                Some(Self::window().request_animation_frame(self.geometry.as_ref().unwrap())?);
        }
        Ok(())
    }
    fn refresh(&mut self) -> Result<Option<serde_json::Value>> {
        self.timer = None;
        self.frame = None;
        if self.disposed || self.armed || self.roles.is_empty() {
            return Ok(None);
        }
        self.last_scan = Self::now();
        let count = highlight(&self.roles, &self.config.variables)?;
        Ok(Some(serde_json::json!({"type":"count","count":count})))
    }
    fn hover_role(&mut self) -> Result<()> {
        self.hover_timer = None;
        if self.disposed || !self.armed {
            return Ok(());
        }
        if let Some(target) = &self.target {
            if target.is_connected() {
                self.inspection = inspect(target, &self.config.roles, &self.config.variables)?;
            }
        }
        self.show_hover()
    }
    fn show_hover(&self) -> Result<()> {
        if let Some((element, role)) = &self.inspection {
            hover(
                element,
                &policy::family(role)
                    .map(|family| family.label.clone())
                    .unwrap_or_else(|| policy::label(role)),
            )?;
        }
        Ok(())
    }
    fn pointer_over(&mut self, event: &web_sys::Event) -> Result<()> {
        if self.disposed || !self.armed {
            return Ok(());
        }
        let Some(target) = event
            .target()
            .and_then(|target| target.dyn_into::<Element>().ok())
        else {
            return self.clear_hover();
        };
        if target.closest(EDITOR)?.is_some() {
            return self.clear_hover();
        }
        self.clear_hover()?;
        self.target = Some(target.clone());
        self.inspection = utilities(&target)?;
        if self.inspection.is_some() {
            self.show_hover()?;
        } else {
            self.hover_timer = Some(
                Self::window().set_timeout_with_callback_and_timeout_and_arguments_0(
                    self.hover.as_ref().unwrap(),
                    140,
                )?,
            );
        }
        Ok(())
    }
    fn pointer_down(&mut self, event: &web_sys::Event) -> Result<Option<serde_json::Value>> {
        if self.disposed || !self.armed {
            return Ok(None);
        }
        let Some(target) = event
            .target()
            .and_then(|target| target.dyn_into::<Element>().ok())
        else {
            return Ok(None);
        };
        if target.closest(EDITOR)?.is_some() {
            return Ok(None);
        }
        event.prevent_default();
        event.stop_propagation();
        if let Some(timer) = self.hover_timer.take() {
            Self::window().clear_timeout_with_handle(timer);
        }
        let inspection = if self.target.as_ref() == Some(&target) && self.inspection.is_some() {
            self.inspection.clone()
        } else {
            inspect(&target, &self.config.roles, &self.config.variables)?
        };
        if let Some((_, role)) = inspection {
            self.clear_hover()?;
            self.clicked = true;
            return Ok(Some(serde_json::json!({"type":"role","role":role})));
        }
        Ok(None)
    }
}
fn notify(
    state: &std::rc::Rc<std::cell::RefCell<State>>,
    result: Result<Option<serde_json::Value>>,
) {
    let event = match result {
        Ok(Some(value)) => value,
        Ok(None) => return,
        Err(cause) => {
            serde_json::json!({"type":"error","message":js_sys::Error::from(cause).to_string().as_string().unwrap_or_else(||"Theme inspector failed".into())})
        }
    };
    let callback = {
        let state = state.borrow();
        if state.disposed {
            return;
        }
        state.callback.clone()
    };
    let _ = callback.call1(&JsValue::UNDEFINED, &event.to_string().into());
}
type Listener = (
    web_sys::EventTarget,
    String,
    wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
);
#[wasm_bindgen]
pub struct ThemeInspector {
    state: std::rc::Rc<std::cell::RefCell<State>>,
    listeners: Vec<Listener>,
    observer: web_sys::MutationObserver,
    _mutation: wasm_bindgen::closure::Closure<dyn FnMut(js_sys::Array)>,
    _work: wasm_bindgen::closure::Closure<dyn FnMut()>,
    _hover: wasm_bindgen::closure::Closure<dyn FnMut()>,
    _geometry: wasm_bindgen::closure::Closure<dyn FnMut()>,
}
#[wasm_bindgen]
impl ThemeInspector {
    #[wasm_bindgen(constructor)]
    pub fn new(raw: &str, callback: js_sys::Function) -> Result<ThemeInspector> {
        use std::{cell::RefCell, rc::Rc};
        use wasm_bindgen::closure::Closure;
        let config: Config = serde_json::from_str(raw).map_err(|cause| error(cause.to_string()))?;
        let state = Rc::new(RefCell::new(State {
            config,
            callback,
            disposed: false,
            armed: false,
            clicked: false,
            roles: vec![],
            target: None,
            inspection: None,
            work: None,
            hover: None,
            geometry: None,
            timer: None,
            frame: None,
            hover_timer: None,
            geometry_frame: None,
            last_scan: 0.,
        }));
        let weak = Rc::downgrade(&state);
        let work = Closure::wrap(Box::new(move || {
            if let Some(state) = weak.upgrade() {
                let result = state.borrow_mut().refresh();
                notify(&state, result);
            }
        }) as Box<dyn FnMut()>);
        let weak = Rc::downgrade(&state);
        let hover = Closure::wrap(Box::new(move || {
            if let Some(state) = weak.upgrade() {
                let result = state.borrow_mut().hover_role();
                notify(&state, result.map(|_| None));
            }
        }) as Box<dyn FnMut()>);
        let weak = Rc::downgrade(&state);
        let geometry = Closure::wrap(Box::new(move || {
            if let Some(state) = weak.upgrade() {
                let result = {
                    let mut state = state.borrow_mut();
                    state.geometry_frame = None;
                    if state.disposed {
                        return;
                    }
                    if state.armed {
                        state.show_hover()
                    } else {
                        refresh_spotlight()
                    }
                };
                notify(&state, result.map(|_| None));
            }
        }) as Box<dyn FnMut()>);
        {
            let mut state = state.borrow_mut();
            state.work = Some(work.as_ref().unchecked_ref::<js_sys::Function>().clone());
            state.hover = Some(hover.as_ref().unchecked_ref::<js_sys::Function>().clone());
            state.geometry = Some(
                geometry
                    .as_ref()
                    .unchecked_ref::<js_sys::Function>()
                    .clone(),
            );
        }
        let weak = Rc::downgrade(&state);
        let mutation = Closure::wrap(Box::new(move |records: js_sys::Array| {
            if records.iter().all(|record| {
                let record: web_sys::MutationRecord = record.unchecked_into();
                let element = record
                    .target()
                    .and_then(|node| node.dyn_into::<Element>().ok());
                element.is_some_and(|element| excluded(&element).unwrap_or(false))
            }) {
                return;
            }
            if let Some(state) = weak.upgrade() {
                let result = state.borrow_mut().schedule_work();
                notify(&state, result.map(|_| None));
            }
        }) as Box<dyn FnMut(js_sys::Array)>);
        let observer = web_sys::MutationObserver::new(mutation.as_ref().unchecked_ref())?;
        // Own every observer/listener before fallible registration.
        let mut surface = ThemeInspector {
            state,
            listeners: vec![],
            observer,
            _mutation: mutation,
            _work: work,
            _hover: hover,
            _geometry: geometry,
        };
        let document: web_sys::EventTarget = document()?.into();
        let window: web_sys::EventTarget = State::window().into();
        for (target, name) in [
            (document.clone(), "pointerover"),
            (document.clone(), "pointerout"),
            (document.clone(), "pointerdown"),
            (document.clone(), "click"),
            (document, "keydown"),
            (window.clone(), "resize"),
            (window, "scroll"),
        ] {
            let weak = Rc::downgrade(&surface.state);
            let kind = name;
            let listener = Closure::wrap(Box::new(move |event: web_sys::Event| {
                if let Some(state) = weak.upgrade() {
                    let result = (|| {
                        let mut s = state.borrow_mut();
                        if s.disposed {
                            return Ok(None);
                        }
                        match kind {
                            "pointerover" => s.pointer_over(&event).map(|_| None),
                            "pointerout" => {
                                if event
                                    .dyn_ref::<web_sys::PointerEvent>()
                                    .is_some_and(|event| event.related_target().is_none())
                                {
                                    s.clear_hover()?;
                                }
                                Ok(None)
                            }
                            "pointerdown" => s.pointer_down(&event),
                            "click" => {
                                if s.armed
                                    && event
                                        .target()
                                        .and_then(|target| target.dyn_into::<Element>().ok())
                                        .is_some_and(|target| {
                                            target.closest(EDITOR).ok().flatten().is_none()
                                        })
                                {
                                    event.prevent_default();
                                    event.stop_propagation();
                                    if s.clicked {
                                        s.clicked = false;
                                        s.armed = false;
                                        s.clear_hover()?;
                                        return Ok(Some(serde_json::json!({"type":"disarmed"})));
                                    }
                                }
                                Ok(None)
                            }
                            "keydown" => {
                                if s.armed
                                    && event
                                        .dyn_ref::<web_sys::KeyboardEvent>()
                                        .is_some_and(|event| event.key() == "Escape")
                                {
                                    event.prevent_default();
                                    s.armed = false;
                                    s.roles.clear();
                                    s.clear_hover()?;
                                    clear_highlights()?;
                                    return Ok(Some(serde_json::json!({"type":"cancel"})));
                                }
                                Ok(None)
                            }
                            "resize" => s.schedule_geometry().map(|_| None),
                            "scroll" => {
                                if s.armed {
                                    s.clear_hover().map(|_| None)
                                } else {
                                    s.schedule_geometry().map(|_| None)
                                }
                            }
                            _ => Ok(None),
                        }
                    })();
                    notify(&state, result);
                }
            }) as Box<dyn FnMut(web_sys::Event)>);
            // Push ownership before registration so constructor failures remove prior listeners.
            surface
                .listeners
                .push((target.clone(), name.into(), listener));
            let listener = &surface.listeners.last().unwrap().2;
            target.add_event_listener_with_callback_and_bool(
                name,
                listener.as_ref().unchecked_ref(),
                true,
            )?;
        }
        Ok(surface)
    }
    pub fn selection(&mut self, raw_roles: &str, armed: bool) -> Result<()> {
        let roles = serde_json::from_str(raw_roles).map_err(|cause| error(cause.to_string()))?;
        let result = {
            let mut state = self.state.borrow_mut();
            if state.disposed {
                return Ok(());
            }
            state.cancel_work();
            state.roles = roles;
            state.armed = armed;
            state.clear_hover()?;
            if armed || state.roles.is_empty() {
                clear_highlights()?;
                Ok(None)
            } else {
                state.refresh()
            }
        };
        self.observer.disconnect();
        let observe = {
            let state = self.state.borrow();
            !state.armed && !state.roles.is_empty()
        };
        if observe {
            let options = web_sys::MutationObserverInit::new();
            options.set_child_list(true);
            options.set_subtree(true);
            let body = document()?
                .body()
                .ok_or_else(|| error("Theme inspector body is unavailable"))?;
            self.observer
                .observe_with_options(body.as_ref(), &options)?;
        }
        notify(&self.state, result);
        Ok(())
    }
    pub fn reveal(&self, role: &str) -> Result<()> {
        let visible = policy::family(role)
            .map(|family| family.role.as_str())
            .unwrap_or(role);
        if !self
            .state
            .borrow()
            .config
            .roles
            .iter()
            .any(|role| role == visible)
        {
            return Ok(());
        }
        if let Some(element) = document()?
            .query_selector(&format!("{EDITOR} [data-theme-color-role=\"{visible}\"]"))?
        {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_behavior(web_sys::ScrollBehavior::Smooth);
            options.set_block(web_sys::ScrollLogicalPosition::Nearest);
            element.scroll_into_view_with_scroll_into_view_options(&options);
        }
        Ok(())
    }
    pub fn dispose(&mut self) {
        if self.state.borrow().disposed {
            return;
        }
        self.observer.disconnect();
        for (target, name, listener) in self.listeners.drain(..) {
            let _ = target.remove_event_listener_with_callback_and_bool(
                &name,
                listener.as_ref().unchecked_ref(),
                true,
            );
        }
        self.state.borrow_mut().dispose();
    }
}
impl Drop for ThemeInspector {
    fn drop(&mut self) {
        self.dispose();
    }
}
