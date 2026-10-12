//! What the server needs of the HTML render helpers shared with the clients (packages/shared/src/htmlRender.ts and the two palettes of
//! themePalettes.ts): the frame limits, the theme a page is handed as CSS custom properties, and the bootstrap injected into a page's head.
//! The clients' side (frame heights, MCP Apps messages, reference parsing) lives in the clients.
use regex::Regex;
use std::sync::OnceLock;

pub const HTML_RENDER_MIN_HEIGHT: i64 = 80;
pub const HTML_RENDER_MAX_HEIGHT: i64 = 2000;
pub const HTML_RENDER_MAX_TITLE_LENGTH: usize = 200;

/// Frame widths the server measures a page at, from phones to the wide chat setting.
pub const HTML_RENDER_MEASURE_WIDTHS: [u32; 9] = [320, 375, 430, 520, 640, 728, 860, 1000, 1144];

/// The reply column's frame width at the default chat width.
pub const HTML_RENDER_COLUMN_WIDTH: u32 = 728;

/// `Math.round`, then the frame limits.
pub fn clamp_html_render_height(height: f64) -> i64 {
    ((height + 0.5).floor() as i64).clamp(HTML_RENDER_MIN_HEIGHT, HTML_RENDER_MAX_HEIGHT)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Fonts {
    pub sans: &'static str,
    pub mono: &'static str,
}

pub const HTML_RENDER_DEFAULT_FONTS: Fonts = Fonts {
    sans: "-apple-system, BlinkMacSystemFont, \"Segoe UI\", system-ui, sans-serif",
    // Concrete names only: some engines alias `ui-monospace` to the proportional UI font.
    mono: "\"SF Mono\", \"SFMono-Regular\", Menlo, Consolas, \"Liberation Mono\", monospace",
};

/// Fonts the server's headless browser lays pages out with. Arial-metric faces stand in for the system UI fonts readers see, so measured heights
/// and preview screenshots wrap text close to how a client will.
pub const HTML_RENDER_MEASURE_FONTS: Fonts = Fonts {
    sans: "\"Liberation Sans\", Arimo, Arial, Helvetica, sans-serif",
    mono: "\"Liberation Mono\", Cousine, Menlo, Consolas, monospace",
};

/// The roles of a theme palette an HTML render is styled with.
pub struct ThemeColors {
    pub canvas: &'static str,
    pub text: &'static str,
    pub muted: &'static str,
    pub muted_foreground: &'static str,
    pub surface: &'static str,
    pub surface_overlay: &'static str,
    pub secondary: &'static str,
    pub secondary_foreground: &'static str,
    pub border: &'static str,
    pub input: &'static str,
    pub focus: &'static str,
    pub message_action: &'static str,
    pub message_action_foreground: &'static str,
    pub accent: &'static str,
    pub accent_foreground: &'static str,
    pub accent_surface: &'static str,
    pub accent_surface_foreground: &'static str,
    pub error: &'static str,
    pub error_foreground: &'static str,
    pub error_surface: &'static str,
    pub warning: &'static str,
    pub warning_foreground: &'static str,
    pub warning_surface: &'static str,
    pub code_background: &'static str,
    pub code_foreground: &'static str,
}

/// The palette T3 Code wears with no theme installed.
pub const T3_CODE_LIGHT_THEME_COLORS: ThemeColors = ThemeColors {
    canvas: "#fcfcfc",
    text: "#27272a",
    muted: "#fafafa",
    muted_foreground: "#71717b",
    surface: "#ffffff",
    surface_overlay: "#ffffff",
    secondary: "#fafafa",
    secondary_foreground: "#27272a",
    border: "#e4e4e7",
    input: "#d4d4d8",
    focus: "#1b4ed8",
    message_action: "#1b4ed8",
    message_action_foreground: "#ffffff",
    accent: "#1b4ed8",
    accent_foreground: "#ffffff",
    accent_surface: "#f4f4f5",
    accent_surface_foreground: "#18181b",
    error: "#fb2c36",
    error_foreground: "#c10007",
    error_surface: "#fcebec",
    warning: "#fe9a00",
    warning_foreground: "#bb4d00",
    warning_surface: "#fcf4e8",
    code_background: "#ffffff",
    code_foreground: "#27272a",
};

pub const T3_CODE_DARK_THEME_COLORS: ThemeColors = ThemeColors {
    canvas: "#0a0a0a",
    text: "#f5f5f5",
    muted: "#111111",
    muted_foreground: "#818181",
    surface: "#111111",
    surface_overlay: "#111111",
    secondary: "#111111",
    secondary_foreground: "#f5f5f5",
    border: "#191919",
    input: "#1e1e1e",
    focus: "#346bf1",
    message_action: "#346bf1",
    message_action_foreground: "#ffffff",
    accent: "#346bf1",
    accent_foreground: "#ffffff",
    accent_surface: "#141414",
    accent_surface_foreground: "#f5f5f5",
    error: "#fb414a",
    error_foreground: "#ff6467",
    error_surface: "#301214",
    warning: "#fe9a00",
    warning_foreground: "#ffb900",
    warning_surface: "#312108",
    code_background: "#111111",
    code_foreground: "#f5f5f5",
};

pub fn theme_colors(appearance: Appearance) -> &'static ThemeColors {
    match appearance {
        Appearance::Light => &T3_CODE_LIGHT_THEME_COLORS,
        Appearance::Dark => &T3_CODE_DARK_THEME_COLORS,
    }
}

/// The resolved theme a client hands an HTML render: its appearance and its custom properties, in order.
pub struct HtmlRenderTheme {
    pub appearance: Appearance,
    pub variables: Vec<(String, String)>,
}

/// Roles themes cannot change, plus a categorical chart series after the theme's own accent.
struct Fixed {
    success: &'static str,
    success_foreground: &'static str,
    info: &'static str,
    info_foreground: &'static str,
    chart: [&'static str; 5],
}

const FIXED_LIGHT: Fixed = Fixed {
    success: "#10b981",
    success_foreground: "#047857",
    info: "#3b82f6",
    info_foreground: "#1d4ed8",
    chart: ["#0d9488", "#d97706", "#9333ea", "#e11d48", "#65a30d"],
};
const FIXED_DARK: Fixed = Fixed {
    success: "#10b981",
    success_foreground: "#34d399",
    info: "#3b82f6",
    info_foreground: "#60a5fa",
    chart: ["#2dd4bf", "#fbbf24", "#c084fc", "#fb7185", "#a3e635"],
};

/// Maps a theme palette to the variables HTML renders style against. Names follow the app's own tokens, except `--accent`, which is the theme's
/// brand color here rather than the app's neutral hover surface.
pub fn html_render_theme(colors: &ThemeColors, appearance: Appearance, fonts: Fonts) -> HtmlRenderTheme {
    let fixed = match appearance {
        Appearance::Light => &FIXED_LIGHT,
        Appearance::Dark => &FIXED_DARK,
    };
    let mut variables: Vec<(String, String)> = [
        ("--background", colors.canvas),
        ("--foreground", colors.text),
        ("--muted", colors.muted),
        ("--muted-foreground", colors.muted_foreground),
        ("--card", colors.surface),
        ("--card-foreground", colors.text),
        ("--popover", colors.surface_overlay),
        ("--popover-foreground", colors.text),
        ("--secondary", colors.secondary),
        ("--secondary-foreground", colors.secondary_foreground),
        ("--border", colors.border),
        ("--input", colors.input),
        ("--ring", colors.focus),
        ("--primary", colors.message_action),
        ("--primary-foreground", colors.message_action_foreground),
        ("--accent", colors.accent),
        ("--accent-foreground", colors.accent_foreground),
        ("--accent-surface", colors.accent_surface),
        ("--accent-surface-foreground", colors.accent_surface_foreground),
        ("--destructive", colors.error),
        ("--destructive-foreground", colors.error_foreground),
        ("--destructive-surface", colors.error_surface),
        ("--warning", colors.warning),
        ("--warning-foreground", colors.warning_foreground),
        ("--warning-surface", colors.warning_surface),
        ("--success", fixed.success),
        ("--success-foreground", fixed.success_foreground),
        ("--info", fixed.info),
        ("--info-foreground", fixed.info_foreground),
        ("--code-background", colors.code_background),
        ("--code-foreground", colors.code_foreground),
        ("--chart-1", colors.accent),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    for (index, color) in fixed.chart.iter().enumerate() {
        variables.push((format!("--chart-{}", index + 2), (*color).to_owned()));
    }
    variables.push(("--radius".into(), "0.625rem".into()));
    variables.push(("--font-sans".into(), fonts.sans.to_owned()));
    variables.push(("--font-mono".into(), fonts.mono.to_owned()));
    HtmlRenderTheme { appearance, variables }
}

/// A JSON string the way `JSON.stringify` writes it.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string is JSON")
}

impl HtmlRenderTheme {
    /// `JSON.stringify` of `{ appearance, variables }`.
    fn to_json(&self) -> String {
        let variables: Vec<String> = self.variables.iter().map(|(name, value)| format!("{}:{}", json_string(name), json_string(value))).collect();
        format!("{{\"appearance\":{},\"variables\":{{{}}}}}", json_string(self.appearance.as_str()), variables.join(","))
    }
}

const THEME_FRAGMENT_KEY: &str = "t3-theme";

/// URL fragment that hands a render its theme before first paint.
pub fn html_render_theme_fragment(theme: &HtmlRenderTheme) -> String {
    format!("#{THEME_FRAGMENT_KEY}={}", crate::asset_access::encode_uri_component(&theme.to_json()))
}

const HOST_CONTEXT_CHANGED_METHOD: &str = "ui/notifications/host-context-changed";
const OPEN_LINK_METHOD: &str = "ui/open-link";
const SIZE_CHANGED_METHOD: &str = "ui/notifications/size-changed";

// The frame scrolls a page taller than itself, but a scrollbar inside the reply reads as a box within the thread, so it stays hidden.
const BASE_CSS: &str = "html{background:var(--background);color:var(--foreground);font-family:var(--font-sans);font-size:14px;line-height:1.5;-webkit-font-smoothing:antialiased;-webkit-text-size-adjust:100%;scrollbar-width:none}html::-webkit-scrollbar{display:none}body{margin:0}code,kbd,pre,samp{font-family:var(--font-mono)}";

fn root_rule(theme: &HtmlRenderTheme) -> String {
    let declarations: String = theme.variables.iter().map(|(name, value)| format!("{name}:{value};")).collect();
    format!(":root{{color-scheme:{};{declarations}}}", theme.appearance.as_str())
}

// Runs synchronously in <head>, before the page's own styles and body, so the first paint is already themed. It rewrites its own <style>
// element rather than setting inline properties, so a page's later `:root` rules still win, then drops the fragment so a page's own hash
// routing never sees it. A link the reader clicks to another page never replaces the page inside the thread: a framed page asks its client to
// open it, and a top-level page (mobile) opens it as a new window, which the client sends to the browser. A framed page also reports its
// content height, measured as the server measures it, so its client can fit the frame to the page.
const BOOTSTRAP_TEMPLATE: &str = r###"(function(){var s=document.getElementById("t3-theme"),n=0;if(!s)return;var b=@BASE_CSS@;function a(t){if(!t||typeof t!=="object"||!t.variables||typeof t.variables!=="object")return;var c=":root{color-scheme:"+(t.appearance==="light"?"light":"dark")+";";for(var k in t.variables){if(/^--[a-z0-9-]+$/.test(k))c+=k+":"+String(t.variables[k]).replace(/[;{}<>]/g,"")+";";}s.textContent=c+"}"+b;}try{var m=/[#&]@KEY@=([^&]*)/.exec(location.hash);if(m){a(JSON.parse(decodeURIComponent(m[1])));history.replaceState(history.state,"",location.pathname+location.search);}}catch(e){}window.addEventListener("message",function(e){var d=e.data,p=d&&d.params;if(d&&d.jsonrpc==="2.0"&&d.method===@HOST@&&p&&p.styles)a({appearance:p.theme,variables:p.styles.variables});});document.addEventListener("click",function(e){var l=e.isTrusted?e.composedPath().find(function(t){return t&&t.matches&&t.matches("a[href]");}):null,u;if(!l)return;try{u=new URL(l.getAttribute("href"),document.baseURI);}catch(x){return;}if(!/^https?:$/.test(u.protocol)||u.href.split("#")[0]===location.href.split("#")[0])return;if(window.parent!==window){e.preventDefault();window.parent.postMessage({jsonrpc:"2.0",id:"t3-link-"+(++n),method:@LINK@,params:{url:u.href}},"*");}else{l.setAttribute("target","_blank");l.setAttribute("rel","noopener");}},true);if(window.parent!==window){var h,o,z=function(){var r=document.documentElement,v=Math.ceil(r.scrollHeight>r.clientHeight?r.scrollHeight:r.getBoundingClientRect().height);if(v===h)return;h=v;window.parent.postMessage({jsonrpc:"2.0",method:@SIZE@,params:{height:v}},"*");};if(window.ResizeObserver){o=new ResizeObserver(z);o.observe(document.documentElement);}document.addEventListener("DOMContentLoaded",function(){if(o&&document.body)o.observe(document.body);z();});window.addEventListener("load",z);}})();"###;

fn bootstrap_script() -> String {
    BOOTSTRAP_TEMPLATE
        .replace("@BASE_CSS@", &json_string(BASE_CSS))
        .replace("@KEY@", THEME_FRAGMENT_KEY)
        .replace("@HOST@", &json_string(HOST_CONTEXT_CHANGED_METHOD))
        .replace("@LINK@", &json_string(OPEN_LINK_METHOD))
        .replace("@SIZE@", &json_string(SIZE_CHANGED_METHOD))
}

/// JavaScript's `\s`: white space and line terminators.
pub(crate) const JS_SPACE: &str = r"[\t\n\x0B\f\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";

struct Patterns {
    charset: Regex,
    viewport: Regex,
    template: Regex,
    head_open: Regex,
    html_open: Regex,
    doctype: Regex,
}

// The tag names match without regard to case in ASCII only, as JavaScript's `i` flag does without `u`.
fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let re = |source: String| Regex::new(&source).expect("a fixed pattern");
        Patterns {
            charset: re(format!("(?i-u:<meta){JS_SPACE}[^>]*(?i-u:charset)")),
            viewport: re(format!("(?i-u:<meta){JS_SPACE}[^>]*(?i-u:name){JS_SPACE}*={JS_SPACE}*[\"']?(?i-u:viewport)")),
            template: re(format!("<(/?)(?i-u:template)(?:{JS_SPACE}[^>]*)?/?>")),
            head_open: re(format!("(?i-u:<head)(?:{JS_SPACE}[^>]*)?>")),
            html_open: re(format!("(?i-u:<html)(?:{JS_SPACE}[^>]*)?>")),
            doctype: re(format!("^{JS_SPACE}*(?i-u:<!doctype)[^>]*>")),
        }
    })
}

pub(crate) fn is_js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// JavaScript's `\b` after an ASCII tag name: whether the next byte is not a word character.
fn word_boundary_after(bytes: &[u8], at: usize) -> bool {
    bytes.get(at).is_none_or(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'))
}

const RAW_TEXT_TAGS: [&str; 9] = ["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript"];

fn starts_with_ignore_case(bytes: &[u8], at: usize, text: &str) -> bool {
    bytes.get(at..at + text.len()).is_some_and(|slice| slice.eq_ignore_ascii_case(text.as_bytes()))
}

/// Where the comment, raw-text element or `<plaintext>` that starts at `at` ends, if one starts there:
/// `/<!--[\s\S]*?(?:-->|$)|<(script|style|…)\b[\s\S]*?(?:<\/\1\s*>|$)|<plaintext\b[\s\S]*$/i`.
fn inert_end(html: &str, at: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    if bytes[at] != b'<' {
        return None;
    }
    if bytes[at..].starts_with(b"<!--") {
        return Some(html[at + 4..].find("-->").map_or(html.len(), |found| at + 4 + found + 3));
    }
    if starts_with_ignore_case(bytes, at + 1, "plaintext") && word_boundary_after(bytes, at + 1 + "plaintext".len()) {
        return Some(html.len());
    }
    let name = RAW_TEXT_TAGS.iter().find(|name| starts_with_ignore_case(bytes, at + 1, name) && word_boundary_after(bytes, at + 1 + name.len()))?;
    // The first `</name` followed by white space and `>`, matched without regard to case, or the end of the page.
    let mut from = at + 1 + name.len();
    while let Some(found) = html[from..].find("</") {
        let close = from + found;
        if starts_with_ignore_case(bytes, close + 2, name) {
            let mut after = close + 2 + name.len();
            while let Some(c) = html[after..].chars().next().filter(|c| is_js_space(*c)) {
                after += c.len_utf8();
            }
            if bytes.get(after) == Some(&b'>') {
                return Some(after + 1);
            }
        }
        from = close + 1;
    }
    Some(html.len())
}

/// Spaces for a run of text, as many bytes as it has, so offsets in the result are offsets in the page.
fn blanks(length: usize) -> String {
    " ".repeat(length)
}

/// The page with comments, raw text and template contents blanked to the same length, so offsets still line up and inert tags cannot receive
/// the bootstrap.
fn blank_non_markup(html: &str) -> String {
    let mut scan = String::with_capacity(html.len());
    let (mut at, mut copied) = (0, 0);
    while at < html.len() {
        match inert_end(html, at) {
            Some(end) => {
                scan.push_str(&html[copied..at]);
                scan.push_str(&blanks(end - at));
                at = end;
                copied = end;
            }
            None => at += 1,
        }
    }
    scan.push_str(&html[copied..]);

    let mut parts: Vec<String> = Vec::new();
    let (mut depth, mut start, mut done) = (0usize, 0usize, 0usize);
    for found in patterns().template.captures_iter(&scan) {
        let whole = found.get(0).expect("a whole match");
        if found.get(1).is_some_and(|slash| slash.as_str().is_empty()) {
            if depth == 0 {
                start = whole.start();
            }
            depth += 1;
        } else if depth > 0 {
            depth -= 1;
            if depth == 0 {
                parts.push(scan[done..start].to_owned());
                parts.push(blanks(whole.end() - start));
                done = whole.end();
            }
        }
    }
    if depth > 0 {
        parts.push(scan[done..start].to_owned());
        parts.push(blanks(scan.len() - start));
        done = scan.len();
    }
    parts.push(scan[done..].to_owned());
    parts.concat()
}

/// The first 4096 UTF-16 units of the blanked page as the original counts them: a blanked character is one space for each unit it had.
fn charset_window(html: &str, scan: &str) -> String {
    let mut window = String::new();
    let mut units = 0;
    for (index, c) in html.char_indices() {
        if units >= 4096 {
            break;
        }
        let end = index + c.len_utf8();
        if scan.as_bytes()[index..end] == html.as_bytes()[index..end] {
            window.push(c);
        } else {
            window.push_str(&" ".repeat(c.len_utf16()));
        }
        units += c.len_utf16();
    }
    window
}

fn bootstrap_markup(html: &str, scan: &str) -> String {
    let dark = html_render_theme(&T3_CODE_DARK_THEME_COLORS, Appearance::Dark, HTML_RENDER_DEFAULT_FONTS);
    let light = html_render_theme(&T3_CODE_LIGHT_THEME_COLORS, Appearance::Light, HTML_RENDER_DEFAULT_FONTS);
    // Without a client-provided theme (a direct download, the headless preview without a fragment) the page follows the OS appearance.
    let default_css = format!("{}@media (prefers-color-scheme: light){{{}}}{BASE_CSS}", root_rule(&dark), root_rule(&light));
    let patterns = patterns();
    [
        if patterns.charset.is_match(&charset_window(html, scan)) { "" } else { "<meta charset=\"utf-8\">" }.to_owned(),
        if patterns.viewport.is_match(scan) { "" } else { "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">" }.to_owned(),
        format!("<style id=\"t3-theme\">{default_css}</style>"),
        format!("<script>{}</script>", bootstrap_script()),
    ]
    .concat()
}

/// Inserts the theme bootstrap at the start of the document head, so a page's own styles and scripts come after it.
pub fn inject_html_render_bootstrap(html: &str) -> String {
    let scan = blank_non_markup(html);
    let markup = bootstrap_markup(html, &scan);
    let patterns = patterns();
    if let Some(head) = patterns.head_open.find(&scan) {
        return format!("{}{markup}{}", &html[..head.end()], &html[head.end()..]);
    }
    if let Some(open) = patterns.html_open.find(&scan) {
        return format!("{}<head>{markup}</head>{}", &html[..open.end()], &html[open.end()..]);
    }
    if let Some(doctype) = patterns.doctype.find(html) {
        return format!("{}<head>{markup}</head>{}", &html[..doctype.end()], &html[doctype.end()..]);
    }
    format!("<!doctype html><head>{markup}</head>{html}")
}
