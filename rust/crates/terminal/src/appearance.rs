//! Appearance font access/probing; shared Rust policy for browser and WebViews.
use crate::{fonts, runtime::Result};
use js_sys::{Function, Reflect};
use serde_json::json;
use std::{cell::RefCell, collections::BTreeSet};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};
pub const SANS_STACK: &str =
    "-apple-system, BlinkMacSystemFont, \"Segoe UI\", system-ui, sans-serif";
pub const CODE_STACK: &str =
    "\"SF Mono\", \"SFMono-Regular\", Menlo, Consolas, \"Liberation Mono\", monospace";
fn context() -> Option<CanvasRenderingContext2d> {
    web_sys::window()?
        .document()?
        .create_element("canvas")
        .ok()?
        .dyn_into::<HtmlCanvasElement>()
        .ok()?
        .get_context("2d")
        .ok()??
        .dyn_into()
        .ok()
}
pub fn available(family: &str) -> bool {
    let families = fonts::quote_families(family);
    if families.is_empty() {
        return false;
    }
    if [
        "system-ui",
        "sans-serif",
        "serif",
        "monospace",
        "ui-monospace",
    ]
    .contains(&families.to_ascii_lowercase().as_str())
    {
        return true;
    }
    let Some(context) = context() else {
        return false;
    };
    for generic in ["monospace", "serif", "sans-serif"] {
        context.set_font(&format!("16px {generic}"));
        let Ok(baseline) = context.measure_text("mmmmmmmmMMWli1O0@# fjord") else {
            return false;
        };
        context.set_font(&format!("16px {families}, {generic}"));
        let Ok(candidate) = context.measure_text("mmmmmmmmMMWli1O0@# fjord") else {
            return false;
        };
        if baseline.width() != candidate.width() {
            return true;
        }
    }
    false
}
pub fn monospace(family: &str) -> bool {
    let families = fonts::quote_families(family);
    if families.is_empty() {
        return true;
    }
    let Some(context) = context() else {
        return true;
    };
    for variant in ["normal 400", "normal 700", "italic 400", "italic 700"] {
        context.set_font(&format!("{variant} 32px {families}, monospace"));
        let advances = ["i", "M", "W", "0", "@", "#", ".", " "]
            .iter()
            .map(|glyph| context.measure_text(glyph).map(|metrics| metrics.width()))
            .collect::<std::result::Result<Vec<_>, _>>();
        match advances {
            Ok(advances) if !fonts::monospace_advances(&advances) => return false,
            Err(_) => return true,
            _ => {}
        }
    }
    true
}
#[wasm_bindgen]
pub fn appearance_probe_font(family: &str) -> String {
    json!({"available":available(family),"monospace":monospace(family)}).to_string()
}
fn dom_width(family: &str) -> Option<f64> {
    let document = web_sys::window()?.document()?;
    let body = document.body()?;
    let span = document
        .create_element("span")
        .ok()?
        .dyn_into::<web_sys::HtmlElement>()
        .ok()?;
    span.style().set_css_text(
        "position:absolute;left:-9999px;top:0;visibility:hidden;white-space:pre;font-size:100px;",
    );
    let _ = span.style().set_property("font-family", family);
    span.set_text_content(Some("RagIl10O@ fjord quiz"));
    body.append_child(&span).ok()?;
    let width = span.get_bounding_client_rect().width();
    span.remove();
    (width > 0.).then_some(width)
}
fn generic_label(generic: &str) -> Option<String> {
    let lower = generic.to_ascii_lowercase();
    if lower == "serif" {
        return None;
    }
    let mono = lower == "monospace" || lower == "ui-monospace";
    let width = dom_width(generic)?;
    let candidates: &[&str] = if mono {
        &[
            "Menlo",
            "Consolas",
            "Cascadia Mono",
            "DejaVu Sans Mono",
            "Ubuntu Mono",
            "Liberation Mono",
            "Noto Sans Mono",
            "Roboto Mono",
            "Monaco",
            "Courier New",
        ]
    } else {
        &[
            "Segoe UI",
            "Roboto",
            "Noto Sans",
            "Ubuntu",
            "Cantarell",
            "DejaVu Sans",
            "Liberation Sans",
            "Helvetica Neue",
            "Arial",
        ]
    };
    for family in candidates {
        if available(family)
            && dom_width(&format!("\"{family}\""))
                .is_some_and(|candidate| (width - candidate).abs() < 0.01)
        {
            return Some((*family).into());
        }
    }
    let platform = web_sys::window()?
        .navigator()
        .platform()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ["mac", "iphone", "ipad", "ipod"]
        .iter()
        .any(|name| platform.contains(name))
    {
        let system = dom_width("-apple-system");
        let system = system.is_some_and(|system| (width - system).abs() < 0.01);
        return Some(if system || !mono { "SF Pro" } else { "SF Mono" }.into());
    }
    None
}
fn default_label(stack: &str) -> Option<String> {
    for raw in stack.split(',') {
        let raw = raw.trim();
        let family = raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(raw);
        if family.is_empty() {
            continue;
        }
        if [
            "system-ui",
            "sans-serif",
            "serif",
            "monospace",
            "ui-monospace",
            "-apple-system",
            "blinkmacsystemfont",
        ]
        .contains(&family.to_ascii_lowercase().as_str())
        {
            if let Some(label) = generic_label(family) {
                return Some(label);
            }
        } else if available(family) {
            return Some(family.into());
        }
    }
    None
}
#[wasm_bindgen]
pub fn appearance_default_fonts() -> String {
    json!({"sans":default_label(SANS_STACK),"code":default_label(CODE_STACK),"enumerationSupported":web_sys::window().and_then(|window|Reflect::get(&window,&"queryLocalFonts".into()).ok()).is_some_and(|query|query.is_function()),"mac":web_sys::window().and_then(|w|w.navigator().platform().ok()).is_some_and(|p|p.to_ascii_lowercase().contains("mac"))}).to_string()
}
thread_local! {static FONTS:RefCell<Option<String>>=const{RefCell::new(None)};}
#[wasm_bindgen]
pub async fn appearance_query_fonts() -> String {
    if let Some(cached) = FONTS.with(|cache| cache.borrow().clone()) {
        return cached;
    }
    let unavailable = || json!({"status":"unsupported","families":[]}).to_string();
    let denied = || json!({"status":"denied","families":[]}).to_string();
    let Some(window) = web_sys::window() else {
        return unavailable();
    };
    let Ok(query) = Reflect::get(&window, &"queryLocalFonts".into())
        .and_then(|query| query.dyn_into::<Function>())
    else {
        let result = unavailable();
        FONTS.with(|cache| *cache.borrow_mut() = Some(result.clone()));
        return result;
    };
    let result = async {
        let promise = query.call0(&window)?;
        let fonts =
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&promise)).await?;
        Ok::<_, JsValue>(js_sys::Array::from(&fonts))
    }
    .await;
    let Ok(fonts) = result else {
        return denied();
    };
    let mut families = BTreeSet::new();
    for font in fonts.iter() {
        if let Ok(value) = Reflect::get(&font, &"family".into()) {
            if let Some(value) = value.as_string() {
                if !value.starts_with('.') {
                    families.insert(value);
                }
            }
        }
    }
    let mut families: Vec<_> = families.into_iter().collect();
    let locales = js_sys::Array::new();
    let options = js_sys::Object::new();
    families.sort_by(|left, right| {
        js_sys::JsString::from(left.as_str())
            .locale_compare(right, &locales, &options)
            .cmp(&0)
    });
    if families.is_empty() {
        return denied();
    }
    let result = json!({"status":"granted","families":families}).to_string();
    FONTS.with(|cache| *cache.borrow_mut() = Some(result.clone()));
    result
}
#[wasm_bindgen]
pub async fn appearance_font_permission() -> String {
    let result = async {
        let window = web_sys::window().ok_or(JsValue::NULL)?;
        let navigator = window.navigator();
        let permissions = Reflect::get(&navigator, &"permissions".into())?;
        let query = Reflect::get(&permissions, &"query".into())?.dyn_into::<Function>()?;
        let request = js_sys::Object::new();
        Reflect::set(&request, &"name".into(), &"local-fonts".into())?;
        let promise = query.call1(&permissions, &request)?;
        let status =
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&promise)).await?;
        Reflect::get(&status, &"state".into())
    };
    result
        .await
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_else(|| "unsupported".into())
}
#[wasm_bindgen]
pub fn appearance_apply_fonts(raw: &str) -> Result<()> {
    let values: serde_json::Value =
        serde_json::from_str(raw).map_err(|cause| JsValue::from_str(&cause.to_string()))?;
    let Some(root) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.document_element())
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return Ok(());
    };
    for (key, variable, fallback) in [
        ("fontFamilySans", "--font-sans", SANS_STACK),
        ("fontFamilyCode", "--font-mono", CODE_STACK),
        ("fontFamilyComposer", "--font-composer", "var(--font-sans)"),
    ] {
        let family = fonts::quote_families(values[key].as_str().unwrap_or(""));
        if family.is_empty() {
            root.style().remove_property(variable)?;
        } else {
            root.style()
                .set_property(variable, &format!("{family}, {fallback}"))?;
        }
    }
    let clamp = |key: &str, min: f64, max: f64, default: f64| {
        values[key]
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| (value + 0.5).floor().clamp(min, max))
            .unwrap_or(default)
    };
    let interface = clamp("fontSizeInterface", 12., 20., 16.);
    let prompt = clamp("fontSizePrompt", 12., 20., 14.);
    let code = clamp("fontSizeCode", 10., 18., 13.);
    root.style()
        .set_property("font-size", &format!("{interface}px"))?;
    root.style()
        .set_property("--font-size-prompt", &format!("{prompt}px"))?;
    root.style()
        .set_property("--font-size-code", &format!("{code}px"))?;
    root.style()
        .set_property("--diffs-font-size", &format!("{code}px"))?;
    if values["fontSmoothing"] == true {
        root.style()
            .set_property("-webkit-font-smoothing", "antialiased")?;
    } else {
        root.style().remove_property("-webkit-font-smoothing")?;
    }
    Ok(())
}

/// Uses the WebView's actual locale, including Turkish/Lithuanian special casing.
#[wasm_bindgen]
pub fn appearance_collection_labels(raw: &str, locale: Option<String>) -> Result<String> {
    let labels: Vec<String> =
        serde_json::from_str(raw).map_err(|cause| JsValue::from_str(&cause.to_string()))?;
    let labels = t3_client::themes::collections::variant_labels(&labels, |word| {
        js_sys::JsString::from(word)
            .to_locale_lower_case(locale.as_deref())
            .as_string()
            .unwrap_or_default()
    });
    Ok(serde_json::to_string(&labels).unwrap())
}
