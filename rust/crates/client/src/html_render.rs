//! An agent's HTML render in the thread: which tool result is one, how tall its frame is, and what the frame is
//! told about the app (its theme) and tells the app back (its height, a link to open). Port of
//! `packages/shared/src/htmlRender.ts` (the client half; the server half is `t3-server`'s `html_render_shared`),
//! of `htmlRenderFromToolItem` (`toolOutput.ts`), and of the font stack of `appearanceFonts.ts`.
//!
//! The page itself runs in a sandboxed frame the UI draws (`html_render.rs` in `t3-ui`); everything that can be
//! decided without a frame is decided here.
use crate::themes::{Appearance, Colors};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const TOOL_NAME: &str = "html_render";
pub const MIN_HEIGHT: u32 = 80;
pub const MAX_HEIGHT: u32 = 2000;
pub const MAX_TITLE_LENGTH: usize = 200;
/// The reply column's frame width at the default chat width. It picks the measured height when a client cannot know its width.
pub const COLUMN_WIDTH: f64 = 728.0;
const MAX_MEASURED_HEIGHTS: usize = 24;

/// What the `html_render` tool result carries so clients can show the page (`HtmlRenderReference`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub attachment_id: String,
    pub title: String,
    /// The agent's frame height in CSS pixels, and the cap on any measured height.
    pub height: u32,
    /// `(width, content height)` pairs the server measured at publish, ascending by width. Absent when the preview browser was
    /// not installed yet.
    pub heights: Option<Vec<(u32, u32)>>,
}

/// `clampHtmlRenderHeight`.
pub fn clamp_height(height: f64) -> u32 {
    // `Math.round` rounds halves up; heights are positive here.
    (height + 0.5).floor().clamp(MIN_HEIGHT as f64, MAX_HEIGHT as f64) as u32
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}
/// The first `limit` UTF-16 units of `text`, without splitting a character (a split cannot be held in a Rust string).
fn utf16_prefix(text: &str, limit: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|character| {
            used += character.len_utf16();
            used <= limit
        })
        .collect()
}

fn read_measured_heights(value: &Value) -> Option<Vec<(u32, u32)>> {
    let list = value.as_array().filter(|list| !list.is_empty() && list.len() <= MAX_MEASURED_HEIGHTS)?;
    let mut heights = Vec::with_capacity(list.len());
    for entry in list {
        let pair = entry.as_array().filter(|pair| pair.len() == 2)?;
        let width = pair[0].as_f64().filter(|width| width.fract() == 0.0 && (1.0..=10_000.0).contains(width))?;
        let height = pair[1].as_f64().filter(|height| height.is_finite())?;
        heights.push((width as u32, clamp_height(height)));
    }
    heights.sort_by_key(|(width, _)| *width);
    Some(heights)
}

/// `readHtmlRenderReference`: the reference in a tool result, if it is a sane one. An unreadable list of measured heights is
/// dropped, not the page.
pub fn read_reference(value: &Value) -> Option<Reference> {
    let attachment_id = value.get("attachmentId")?.as_str().filter(|id| !id.is_empty() && utf16_len(id) <= 256)?;
    let title = value.get("title")?.as_str()?;
    let height = value.get("height")?.as_f64().filter(|height| height.is_finite())?;
    let title = utf16_prefix(title.trim(), MAX_TITLE_LENGTH);
    Some(Reference {
        attachment_id: attachment_id.to_owned(),
        title: if title.is_empty() { "HTML".to_owned() } else { title },
        height: clamp_height(height),
        heights: value.get("heights").and_then(read_measured_heights),
    })
}

/// The page a completed `html_render` tool call published, if this turn item is one (`htmlRenderFromToolItem`, and the
/// `dynamic_tool` + `completed` test of `session-logic.ts`). A call whose result says it failed shows as the tool call it is.
pub fn from_item(item: &Value) -> Option<Reference> {
    if item["type"] != "dynamic_tool" || item["status"] != "completed" {
        return None;
    }
    if crate::t3_tools::resolve(item["toolName"].as_str()).is_none_or(|tool| tool.id != TOOL_NAME) {
        return None;
    }
    read_reference(&crate::t3_tools::output_html_render(&item["output"])?)
}

// The taller of the heights measured at the nearest widths on each side. A breakpoint between two measured widths can make
// the page as tall as either.
fn measured_height(heights: &[(u32, u32)], width: f64) -> u32 {
    let high = heights.iter().position(|(measured, _)| *measured as f64 >= width).unwrap_or(heights.len() - 1);
    let low = if heights[high].0 as f64 == width { high } else { high.saturating_sub(1) };
    heights[low].1.max(heights[high].1)
}

/// `htmlRenderFrameHeight`: the frame height for a page at a frame width. It is the page's own reported `content_height` when
/// the client has one, else the server's measurement for that width. A page even a few pixels taller than its frame scrolls
/// inside it and takes the reader's scroll, so the frame fits the page. The agent's height caps it only when it is below the
/// page's height at the column width (the agent asked for a scrolling frame) or when the page was never measured.
pub fn frame_height(reference: &Reference, width: f64, content_height: Option<f64>) -> u32 {
    let Some(heights) = reference.heights.as_deref().filter(|heights| !heights.is_empty()) else {
        return clamp_height((reference.height as f64).min(content_height.unwrap_or(reference.height as f64)));
    };
    let cap = if measured_height(heights, COLUMN_WIDTH) > reference.height { reference.height } else { MAX_HEIGHT };
    clamp_height((cap as f64).min(content_height.unwrap_or_else(|| measured_height(heights, width) as f64)))
}

/// `htmlRenderFileName`: a readable download name, the title without characters file systems reject.
pub fn file_name(title: &str) -> String {
    let mut name = String::new();
    let mut pending_space = false;
    for character in title.chars() {
        // `[\\/:*?"<>|\p{Cc}]+` becomes one space, and any whitespace run one space.
        if matches!(character, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || character.is_control() || character.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !name.is_empty() {
                name.push(' ');
            }
            pending_space = false;
            name.push(character);
        }
    }
    let name = utf16_prefix(&name, 120);
    let name = name.trim();
    format!("{}.html", if name.is_empty() { "Page" } else { name })
}

// ---- the theme a page wears -------------------------------------------------------------------------------------------

pub const DEFAULT_SANS_FONT_STACK: &str = "-apple-system, BlinkMacSystemFont, \"Segoe UI\", system-ui, sans-serif";
// Concrete names first: some engines alias `ui-monospace` to the proportional system UI font.
pub const DEFAULT_CODE_FONT_STACK: &str = "\"SF Mono\", \"SFMono-Regular\", Menlo, Consolas, \"Liberation Mono\", monospace";

fn quote_font_family_name(name: &str) -> String {
    let bare = name.trim();
    if bare.is_empty() {
        return String::new();
    }
    let quoted = bare.len() >= 2 && matches!(bare.chars().next(), Some('\'' | '"')) && bare.chars().next() == bare.chars().last();
    let ident = bare.chars().next().is_some_and(|first| first.is_ascii_alphabetic()) && bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if quoted || ident { bare.to_owned() } else { format!("\"{}\"", bare.replace('"', "")) }
}
/// `cssFontFamilies`: a user-entered family (one name or a comma-separated list) as a safe CSS list, or None when it is empty.
pub fn css_font_families(input: &str) -> Option<String> {
    let families: Vec<String> = input.split(',').map(quote_font_family_name).filter(|name| !name.is_empty()).collect();
    (!families.is_empty()).then(|| families.join(", "))
}
/// `appearanceFontStack`: the full stack a preference resolves to, custom families before the default.
pub fn font_stack(custom: &str, default_stack: &str) -> String {
    css_font_families(custom).map_or_else(|| default_stack.to_owned(), |families| format!("{families}, {default_stack}"))
}

/// The resolved theme a client hands an HTML render (`HtmlRenderTheme`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub appearance: Appearance,
    pub variables: BTreeMap<String, String>,
}

struct Fixed {
    success: &'static str,
    success_foreground: &'static str,
    info: &'static str,
    info_foreground: &'static str,
    chart: [&'static str; 5],
}
// Roles themes cannot change (index.css keeps info/success fixed), plus a categorical chart series after the theme's own accent.
const FIXED_LIGHT: Fixed = Fixed { success: "#10b981", success_foreground: "#047857", info: "#3b82f6", info_foreground: "#1d4ed8", chart: ["#0d9488", "#d97706", "#9333ea", "#e11d48", "#65a30d"] };
const FIXED_DARK: Fixed = Fixed { success: "#10b981", success_foreground: "#34d399", info: "#3b82f6", info_foreground: "#60a5fa", chart: ["#2dd4bf", "#fbbf24", "#c084fc", "#fb7185", "#a3e635"] };

/// `htmlRenderTheme`: maps a theme palette to the variables HTML renders style against. Names follow the app's own tokens,
/// except `--accent`, which is the theme's brand color here rather than the app's neutral hover surface. A role the palette
/// lacks is left out, so the page's base stylesheet keeps its own default for it.
pub fn theme(colors: &Colors, appearance: Appearance, sans: &str, mono: &str) -> Theme {
    let fixed = if appearance == Appearance::Light { FIXED_LIGHT } else { FIXED_DARK };
    let mut variables = BTreeMap::new();
    let mut put = |name: &str, value: Option<&str>| {
        if let Some(value) = value {
            variables.insert(name.to_owned(), value.to_owned());
        }
    };
    let role = |name: &str| colors.get(name).map(String::as_str);
    for (variable, source) in [
        ("--background", "canvas"),
        ("--foreground", "text"),
        ("--muted", "muted"),
        ("--muted-foreground", "mutedForeground"),
        ("--card", "surface"),
        ("--card-foreground", "text"),
        ("--popover", "surfaceOverlay"),
        ("--popover-foreground", "text"),
        ("--secondary", "secondary"),
        ("--secondary-foreground", "secondaryForeground"),
        ("--border", "border"),
        ("--input", "input"),
        ("--ring", "focus"),
        ("--primary", "messageAction"),
        ("--primary-foreground", "messageActionForeground"),
        ("--accent", "accent"),
        ("--accent-foreground", "accentForeground"),
        ("--accent-surface", "accentSurface"),
        ("--accent-surface-foreground", "accentSurfaceForeground"),
        ("--destructive", "error"),
        ("--destructive-foreground", "errorForeground"),
        ("--destructive-surface", "errorSurface"),
        ("--warning", "warning"),
        ("--warning-foreground", "warningForeground"),
        ("--warning-surface", "warningSurface"),
        ("--code-background", "codeBackground"),
        ("--code-foreground", "codeForeground"),
        ("--chart-1", "accent"),
    ] {
        put(variable, role(source));
    }
    put("--success", Some(fixed.success));
    put("--success-foreground", Some(fixed.success_foreground));
    put("--info", Some(fixed.info));
    put("--info-foreground", Some(fixed.info_foreground));
    for (index, color) in fixed.chart.iter().enumerate() {
        put(&format!("--chart-{}", index + 2), Some(color));
    }
    put("--radius", Some("0.625rem"));
    put("--font-sans", Some(sans));
    put("--font-mono", Some(mono));
    Theme { appearance, variables }
}
impl Theme {
    fn wire(&self) -> Value {
        json!({"appearance":self.appearance.key(),"variables":self.variables})
    }
}

/// `encodeURIComponent`: everything but letters, digits and `-_.!~*'()` as percent-encoded UTF-8.
fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// `htmlRenderThemeFragment`: the URL fragment that hands a render its theme before first paint.
pub fn theme_fragment(theme: &Theme) -> String {
    let text = serde_json::to_string(&theme.wire()).expect("theme json");
    format!("#t3-theme={}", encode_uri_component(&text))
}
/// The address a render is loaded from: the signed URL without any fragment of its own, then the theme's.
pub fn framed_url(url: &str, theme: &Theme) -> String {
    format!("{}{}", url.split('#').next().unwrap_or(url), theme_fragment(theme))
}
/// `htmlRenderThemeMessage`: the `host-context-changed` notification posted into a mounted render when the theme changes.
pub fn theme_message(theme: &Theme) -> Value {
    json!({"jsonrpc":"2.0","method":"ui/notifications/host-context-changed","params":{"theme":theme.appearance.key(),"styles":{"variables":theme.variables}}})
}

// ---- what a page says back ---------------------------------------------------------------------------------------------

/// `readHtmlRenderContentHeight`: the content height in a framed render's `ui/notifications/size-changed` notification.
pub fn read_content_height(data: &Value) -> Option<f64> {
    if data["jsonrpc"] != "2.0" || data["method"] != "ui/notifications/size-changed" {
        return None;
    }
    data["params"]["height"].as_f64().filter(|height| height.is_finite() && *height > 0.0)
}

/// A render's `ui/open-link` request.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRequest {
    /// The request id, to send back in the answer.
    pub id: Value,
    pub url: String,
}
/// `readHtmlRenderLinkRequest`: a render's `ui/open-link` request, if `data` is one with an http(s) URL.
pub fn read_link_request(data: &Value) -> Option<LinkRequest> {
    if data["jsonrpc"] != "2.0" || data["method"] != "ui/open-link" {
        return None;
    }
    let id = data.get("id").filter(|id| id.is_string() || id.is_number())?.clone();
    let url = data["params"]["url"].as_str()?;
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")).then(|| LinkRequest { id, url: url.to_owned() })
}
/// `htmlRenderResult`: the empty result sent back for a render's request.
pub fn result_message(id: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":{}})
}

/// Whether a render's open-link request may be carried out: only while its frame has focus and the reader has just used the app
/// (`BrowserDocumentFrame.tsx`). A page can take focus by script, so this stops opens on load, not a page that waits for the
/// reader's next click or key. `user_active` is `None` where the browser has no `navigator.userActivation`.
pub fn may_open_link(frame_focused: bool, user_active: Option<bool>) -> bool {
    frame_focused && user_active != Some(false)
}

/// The sandbox of the frame a render runs in. Never `allow-same-origin`: the opaque origin keeps the page out of the app's
/// session and storage. Popups are not allowed either; a page asks the app to open a link (`read_link_request`).
pub const SANDBOX: &str = "allow-scripts allow-forms";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::themes::Catalog;

    fn reference(value: Value) -> Reference {
        read_reference(&value).expect("reference")
    }

    #[test]
    fn a_reference_is_clamped_and_its_measured_heights_sorted_or_dropped_whole() {
        let page = reference(json!({"attachmentId":"a","title":"  A page  ","height":5000,"heights":[[640,300.4],[320,50]]}));
        assert_eq!((page.title.as_str(), page.height), ("A page", MAX_HEIGHT));
        assert_eq!(page.heights, Some(vec![(320, MIN_HEIGHT), (640, 300)]));
        // One unreadable entry drops the whole list, never the page.
        let page = reference(json!({"attachmentId":"a","title":"","height":300,"heights":[[320,100],[3.5,100]]}));
        assert_eq!((page.title.as_str(), page.heights), ("HTML", None));
        for bad in [json!({"title":"x","height":1}), json!({"attachmentId":"","title":"x","height":1}), json!({"attachmentId":"a","title":1,"height":1}), json!({"attachmentId":"a","title":"x"})] {
            assert!(read_reference(&bad).is_none(), "{bad}");
        }
        assert_eq!(reference(json!({"attachmentId":"a","title":"é".repeat(300),"height":100})).title.chars().count(), MAX_TITLE_LENGTH);
    }

    #[test]
    fn the_frame_fits_the_page_and_the_agents_height_caps_it_only_when_the_page_scrolls_by_request() {
        let measured = reference(json!({"attachmentId":"a","title":"t","height":600,"heights":[[320,900],[728,500],[1144,400]]}));
        // The server's measurement for the width, with the page's own report winning.
        assert_eq!(frame_height(&measured, 728.0, None), 500);
        // The agent's 600 caps only when the column width's own height is above it; here it is 500.
        assert_eq!(frame_height(&measured, 320.0, None), 900);
        assert_eq!(frame_height(&measured, 500.0, Some(450.0)), 450);
        // Between two measured widths the taller one counts.
        assert_eq!(frame_height(&measured, 1000.0, None), 500);
        // A page the agent asked to scroll (its height is below the column's) stays at the agent's height.
        let scrolling = reference(json!({"attachmentId":"a","title":"t","height":300,"heights":[[728,900]]}));
        assert_eq!(frame_height(&scrolling, 728.0, Some(900.0)), 300);
        // A page that is taller than the agent said fits itself when the agent's height is the column's.
        let tall = reference(json!({"attachmentId":"a","title":"t","height":1000,"heights":[[728,900]]}));
        assert_eq!(frame_height(&tall, 728.0, Some(1500.0)), 1500);
        // Never measured: the agent's height, or the page's if it is shorter.
        let unmeasured = reference(json!({"attachmentId":"a","title":"t","height":400}));
        assert_eq!((frame_height(&unmeasured, 728.0, None), frame_height(&unmeasured, 728.0, Some(200.0)), frame_height(&unmeasured, 728.0, Some(900.0))), (400, 200, 400));
    }

    #[test]
    fn a_download_name_drops_what_file_systems_reject() {
        assert_eq!(file_name("Q3: plan / draft?"), "Q3 plan draft.html");
        assert_eq!(file_name("   "), "Page.html");
        assert_eq!(file_name("a\u{0}b\tc"), "a b c.html");
        assert_eq!(file_name(&"x".repeat(200)).len(), 120 + ".html".len());
    }

    #[test]
    fn only_a_completed_successful_html_render_call_is_a_page() {
        let output = json!({"structuredContent":{"htmlRender":{"attachmentId":"page","title":"Chart","height":300},"message":"shown"},"isError":false});
        let call = |name: &str, status: &str, output: Value| json!({"type":"dynamic_tool","toolName":name,"status":status,"output":output});
        for name in ["html_render", "mcp__t3-code__html_render", "t3-code.html_render"] {
            assert_eq!(from_item(&call(name, "completed", output.clone())).map(|page| page.attachment_id), Some("page".to_owned()), "{name}");
        }
        assert!(from_item(&call("html_render", "running", output.clone())).is_none());
        assert!(from_item(&call("other_tool", "completed", output.clone())).is_none());
        let mut failed = output.clone();
        failed["isError"] = json!(true);
        assert!(from_item(&call("html_render", "completed", failed)).is_none());
        // The compact wire form and a JSON string are read alike.
        let compact = json!({"htmlRender":{"attachmentId":"page","title":"Chart","height":300}});
        assert!(from_item(&call("html_render", "completed", compact.clone())).is_some());
        assert!(from_item(&call("html_render", "completed", json!(compact.to_string()))).is_some());
        assert!(from_item(&json!({"type":"command_execution","toolName":"html_render","status":"completed","output":compact})).is_none());
    }

    #[test]
    fn the_theme_maps_the_palette_and_travels_in_the_url_fragment_as_the_original_encodes_it() {
        let catalog = Catalog::default();
        let colors = catalog.data.standard[&Appearance::Dark].clone();
        let dark = theme(&colors, Appearance::Dark, "S", "M");
        assert_eq!(dark.variables["--background"], colors["canvas"]);
        assert_eq!(dark.variables["--primary"], colors["messageAction"]);
        assert_eq!(dark.variables["--chart-1"], colors["accent"]);
        assert_eq!(dark.variables["--chart-2"], "#2dd4bf");
        assert_eq!(dark.variables["--success-foreground"], "#34d399");
        assert_eq!((dark.variables["--font-sans"].as_str(), dark.variables["--radius"].as_str()), ("S", "0.625rem"));
        let light = theme(&catalog.data.standard[&Appearance::Light], Appearance::Light, "S", "M");
        assert_eq!(light.variables["--success-foreground"], "#047857");
        let fragment = theme_fragment(&dark);
        assert!(fragment.starts_with("#t3-theme=%7B%22appearance%22%3A%22dark%22%2C%22variables%22%3A%7B"), "{fragment}");
        assert!(!fragment.contains(' ') && !fragment.contains('"'));
        assert_eq!(framed_url("https://h/a?sig=1#old", &dark), format!("https://h/a?sig=1{fragment}"));
        let message = theme_message(&dark);
        assert_eq!((message["method"].as_str(), message["params"]["theme"].as_str()), (Some("ui/notifications/host-context-changed"), Some("dark")));
    }

    #[test]
    fn a_page_can_only_resize_its_frame_or_ask_for_an_http_link_and_a_link_needs_a_focused_active_frame() {
        let size = |height: Value| json!({"jsonrpc":"2.0","method":"ui/notifications/size-changed","params":{"height":height}});
        assert_eq!(read_content_height(&size(json!(412.5))), Some(412.5));
        for bad in [size(json!(0)), size(json!(-3)), size(json!("9")), json!({"jsonrpc":"1.0","method":"ui/notifications/size-changed","params":{"height":5}}), json!(null)] {
            assert_eq!(read_content_height(&bad), None, "{bad}");
        }
        let open = |id: Value, url: &str| json!({"jsonrpc":"2.0","id":id,"method":"ui/open-link","params":{"url":url}});
        assert_eq!(read_link_request(&open(json!("t3-link-1"), "https://example.com/a")), Some(LinkRequest { id: json!("t3-link-1"), url: "https://example.com/a".into() }));
        assert!(read_link_request(&open(json!(7), "HTTP://example.com")).is_some());
        for bad in [open(json!("1"), "javascript:alert(1)"), open(json!("1"), "file:///etc/passwd"), open(json!("1"), "data:text/html,x"), open(json!(null), "https://x"), open(json!(true), "https://x")] {
            assert!(read_link_request(&bad).is_none(), "{bad}");
        }
        assert_eq!(result_message(&json!(7)), json!({"jsonrpc":"2.0","id":7,"result":{}}));
        assert!(may_open_link(true, None) && may_open_link(true, Some(true)));
        assert!(!may_open_link(false, Some(true)) && !may_open_link(true, Some(false)));
        assert_eq!(SANDBOX, "allow-scripts allow-forms");
    }

    #[test]
    fn a_custom_font_goes_before_the_default_stack() {
        assert_eq!(font_stack("", "d"), "d");
        assert_eq!(font_stack(" Fira Code , \"Inter\", 'x y'", "d"), "\"Fira Code\", \"Inter\", 'x y', d");
        assert_eq!(font_stack("Arial", "d"), "Arial, d");
        assert_eq!(font_stack("a\"b c", "d"), "\"ab c\", d");
    }
}
