//! HTML renders (apps/server/src/htmlRender/HtmlRender.ts): the self-contained pages an agent publishes into a thread with `html_render`, and
//! the screenshots it checks them with in `html_preview`. A page's local images are inlined as data URIs, the theme bootstrap is injected, and the
//! pinned headless shell (`preview_browser_install.rs`) renders and measures it.
use crate::{
    attachment_store,
    headless_chrome::{self, BrowserLauncher, ConsoleMessage, HtmlRenderBrowserError, ProcessLauncher},
    html_render_shared::{
        Appearance, HTML_RENDER_COLUMN_WIDTH, HTML_RENDER_MAX_TITLE_LENGTH, HTML_RENDER_MEASURE_FONTS, HTML_RENDER_MEASURE_WIDTHS, T3_CODE_DARK_THEME_COLORS, clamp_html_render_height, html_render_theme,
        html_render_theme_fragment, inject_html_render_bootstrap, is_js_space, theme_colors,
    },
    persistence::Store,
    preview_browser_host::{self, PreviewBrowserHostError},
    preview_browser_install::{PreviewBrowser, PreviewBrowserError, PreviewBrowserOptions},
};
use async_trait::async_trait;
use base64::Engine;
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, sync::Semaphore};

const MIB: u64 = 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 10 * MIB;
const MAX_PAGE_BYTES: u64 = 25 * MIB;

/// `(bytes / MiB).toFixed(1)` MiB: tenths rounded half up.
fn format_mib(bytes: u64) -> String {
    let tenths = (u128::from(bytes) * 10 + u128::from(MIB) / 2) / u128::from(MIB);
    format!("{}.{} MiB", tenths / 10, tenths % 10)
}

/// Why a page could not be prepared, stored or rendered. Every message is built on the server and tells the agent what to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum HtmlRenderError {
    ImagesNotFound { paths: Vec<String> },
    ImageTooLarge { path: String, size_bytes: u64 },
    PageTooLarge { size_bytes: u64 },
    Store { cause: String },
    Install(PreviewBrowserError),
    Host(PreviewBrowserHostError),
    Browser(HtmlRenderBrowserError),
}

impl HtmlRenderError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::ImagesNotFound { .. } => "HtmlRenderImagesNotFoundError",
            Self::ImageTooLarge { .. } => "HtmlRenderImageTooLargeError",
            Self::PageTooLarge { .. } => "HtmlRenderPageTooLargeError",
            Self::Store { .. } => "HtmlRenderStoreError",
            Self::Install(error) => error.tag(),
            Self::Host(error) => error.tag(),
            Self::Browser(_) => "HtmlRenderBrowserError",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::ImagesNotFound { paths } => format!("These local images could not be read: {}. Use absolute paths to existing image files, or remove them.", paths.join(", ")),
            Self::ImageTooLarge { path, size_bytes } => format!("{path} is {}; each local image must be at most {}.", format_mib(*size_bytes), format_mib(MAX_IMAGE_BYTES)),
            Self::PageTooLarge { size_bytes } => format!("With its images inlined the page is {}; the limit is {}. Use smaller images.", format_mib(*size_bytes), format_mib(MAX_PAGE_BYTES)),
            Self::Store { .. } => "The HTML render could not be saved.".into(),
            Self::Install(error) => error.message(),
            Self::Host(error) => error.message(),
            Self::Browser(error) => error.message(),
        }
    }

    /// A page the agent has to fix, as opposed to a failure of the server.
    pub fn invalid_page(&self) -> bool {
        matches!(self, Self::ImagesNotFound { .. } | Self::ImageTooLarge { .. } | Self::PageTooLarge { .. })
    }
}
impl std::fmt::Display for HtmlRenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}
impl std::error::Error for HtmlRenderError {}

const IMAGE_MIME_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("svg", "image/svg+xml"),
    ("bmp", "image/bmp"),
    ("ico", "image/x-icon"),
];

/// What a reference to a local image in a page spans, and the path it names.
#[derive(Debug, PartialEq)]
pub(crate) struct ImageReference {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) path: String,
}

/// The longest a path can run in bytes: 2048 UTF-16 units, three bytes at most each, and the extension.
const MAX_PATH_SCAN: usize = 2048 * 3 + 8;

fn starts_with_ignore_case(bytes: &[u8], at: usize, text: &str) -> bool {
    bytes.get(at..at + text.len()).is_some_and(|slice| slice.eq_ignore_ascii_case(text.as_bytes()))
}

/// Where an absolute path's first part ends at `at`: POSIX `/…` (not protocol-relative `//…`) or Windows `C:\…` / `C:/…`.
fn absolute_prefix_end(bytes: &[u8], at: usize) -> Option<usize> {
    match bytes.get(at)? {
        b'/' => (bytes.get(at + 1) != Some(&b'/')).then_some(at + 1),
        letter if letter.is_ascii_alphabetic() => (bytes.get(at + 1) == Some(&b':') && matches!(bytes.get(at + 2), Some(b'\\' | b'/'))).then_some(at + 3),
        _ => None,
    }
}

/// Where the image extension that ends `text` begins, its dot: one of the image extensions without regard to case.
fn extension_dot(text: &[u8]) -> Option<usize> {
    IMAGE_MIME_TYPES.iter().find_map(|(extension, _)| {
        let dot = text.len().checked_sub(extension.len() + 1)?;
        (text[dot] == b'.' && text[dot + 1..].eq_ignore_ascii_case(extension.as_bytes())).then_some(dot)
    })
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `(["'`])(ABS(?:(?!\1)[^\r\n]){0,2048}?\.(?:ext))\1` at a quote: an absolute image path that is a whole quoted string. Returns the path's span
/// and where the match ends.
fn quoted_image(html: &str, quote_at: usize) -> Option<(usize, usize, usize)> {
    let bytes = html.as_bytes();
    let quote = bytes[quote_at];
    let start = quote_at + 1;
    let body = absolute_prefix_end(bytes, start)?;
    // The path cannot cross its own quote or a line end, so the first of them is where it must close.
    let window = &bytes[body..bytes.len().min(body + MAX_PATH_SCAN)];
    let close = body + window.iter().position(|byte| *byte == quote || *byte == b'\r' || *byte == b'\n')?;
    if bytes[close] != quote {
        return None;
    }
    let dot = body + extension_dot(&bytes[body..close])?;
    (units(&html[body..dot]) <= 2048).then_some((start, close, close + 1))
}

/// `url\(\s*(ABS[^\s"'`()]{0,2048}?\.(?:ext))\s*\)` at a `url(`: an unquoted CSS url.
fn css_url_image(html: &str, at: usize) -> Option<(usize, usize, usize)> {
    let bytes = html.as_bytes();
    if !starts_with_ignore_case(bytes, at, "url(") {
        return None;
    }
    let skip_space = |mut from: usize| {
        while let Some(c) = html[from..].chars().next().filter(|c| is_js_space(*c)) {
            from += c.len_utf8();
        }
        from
    };
    let start = skip_space(at + 4);
    let body = absolute_prefix_end(bytes, start)?;
    // The path runs to the first space, quote or parenthesis, and its extension must end there.
    let end = html[body..].char_indices().find(|(_, c)| is_js_space(*c) || matches!(c, '"' | '\'' | '`' | '(' | ')')).map_or(html.len(), |(offset, _)| body + offset);
    if end - body > MAX_PATH_SCAN {
        return None;
    }
    let dot = body + extension_dot(&bytes[body..end])?;
    if units(&html[body..dot]) > 2048 {
        return None;
    }
    let close = skip_space(end);
    (bytes.get(close) == Some(&b')')).then_some((start, end, close + 1))
}

/// Every absolute image path in the page that is a whole quoted string ("…", '…', `…`) or an unquoted CSS url(…). URLs, data:, blob:, and
/// relative paths never match.
pub(crate) fn find_local_images(html: &str) -> Vec<ImageReference> {
    let bytes = html.as_bytes();
    let mut references = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let found = match bytes[at] {
            b'"' | b'\'' | b'`' => quoted_image(html, at),
            b'u' | b'U' => css_url_image(html, at),
            _ => None,
        };
        match found {
            Some((start, end, resume)) => {
                references.push(ImageReference { start, end, path: html[start..end].to_owned() });
                at = resume;
            }
            None => at += 1,
        }
    }
    references
}

/// Inside a JS string literal a Windows path's backslashes are escaped.
pub(crate) fn file_path_for(reference: &str) -> String {
    let drive = reference.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && reference.as_bytes().get(1) == Some(&b':');
    if drive { reference.replace("\\\\", "\\") } else { reference.to_owned() }
}

pub(crate) fn data_uri_prefix(path: &str) -> String {
    let extension = path.rsplit_once('.').map_or(path, |(_, extension)| extension).to_ascii_lowercase();
    let mime = IMAGE_MIME_TYPES.iter().find(|(known, _)| *known == extension).map_or("application/octet-stream", |(_, mime)| *mime);
    format!("data:{mime};base64,")
}

/// The index just past `token` at or after `from`, or None when it never appears.
fn after(text: &str, token: &str, from: usize) -> Option<usize> {
    text.get(from..)?.find(token).map(|found| from + found + token.len())
}

/// The index just past a doctype whose body starts at `from`, honoring quotes and its internal subset.
fn after_doctype(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let (mut in_subset, mut at) = (false, from);
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'"' || byte == b'\'' {
            at = after(text, &(byte as char).to_string(), at + 1)?;
        } else if in_subset && bytes[at..].starts_with(b"<!--") {
            at = after(text, "-->", at + 4)?;
        } else if in_subset && bytes[at..].starts_with(b"<?") {
            at = after(text, "?>", at + 2)?;
        } else if byte == b'>' && !in_subset {
            return Some(at + 1);
        } else {
            if byte == b'[' {
                in_subset = true;
            } else if byte == b']' {
                in_subset = false;
            }
            at += 1;
        }
    }
    None
}

/// Whether an XML document's root element is <svg>, after any processing instructions, comments, and a doctype. One forward pass, so no input can
/// make it slow, and quoted text never counts as markup.
pub(crate) fn has_svg_root(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = Some(0);
    while let Some(mut here) = at {
        while let Some(c) = text.get(here..).and_then(|rest| rest.chars().next()).filter(|c| is_js_space(*c)) {
            here += c.len_utf8();
        }
        if bytes[here.min(bytes.len())..].starts_with(b"<?") {
            at = after(text, "?>", here + 2);
        } else if bytes[here.min(bytes.len())..].starts_with(b"<!--") {
            at = after(text, "-->", here + 4);
        } else if starts_with_ignore_case(bytes, here, "<!doctype") {
            at = after_doctype(text, here + 9);
        } else {
            // XML names are case-sensitive, and only these characters can end one here.
            return bytes[here.min(bytes.len())..].starts_with(b"<svg") && matches!(bytes.get(here + 4), Some(b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>'));
        }
    }
    false
}

/// Whether file bytes are an image, whatever the file is named, so a symlink or renamed file cannot carry other data, such as a secret, into a page.
pub(crate) fn is_image_bytes(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(12)];
    let at = |from: usize, to: usize| head.get(from..to.min(head.len())).unwrap_or_default();
    if head.starts_with(b"\x89PNG")
        || head.starts_with(b"\xff\xd8\xff")
        || head.starts_with(b"GIF8")
        || head.starts_with(b"\0\0\x01\0")
        || (head.starts_with(b"BM") && at(6, 10) == b"\0\0\0\0")
        || (head.starts_with(b"RIFF") && at(8, 12) == b"WEBP")
        || (at(4, 8) == b"ftyp" && matches!(at(8, 12), b"avif" | b"avis" | b"mif1"))
    {
        return true;
    }
    has_svg_root(&String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]))
}

#[derive(Debug)]
struct Inlined {
    html: String,
    /// Local image paths that could not be read; they stay as written.
    missing: Vec<String>,
}

/// A file's size when it is a regular file.
async fn regular_file_size(path: &str) -> Option<u64> {
    let metadata = tokio::fs::metadata(file_path_for(path)).await.ok()?;
    metadata.is_file().then_some(metadata.len())
}

/// At most `MAX_IMAGE_BYTES + 1` bytes of a regular file, or none when it cannot be read as one. A file swapped for a pipe after its size was read
/// is not waited on.
async fn read_image(path: &str) -> Option<Vec<u8>> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(file_path_for(path)).await.ok()?;
    if !file.metadata().await.ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes).await.ok()?;
    Some(bytes)
}

/// Replaces every local image reference with a data URI; unreadable paths stay as written.
async fn inline_local_images(html: &str) -> Result<Inlined, HtmlRenderError> {
    let references = find_local_images(html);
    let mut seen = HashSet::new();
    let unique: Vec<&str> = references.iter().map(|reference| reference.path.as_str()).filter(|path| seen.insert(*path)).collect();
    // Owned paths, so the futures are not tied to a borrow of the page and can run on any thread.
    let files: Vec<(String, Option<u64>)> = stream::iter(unique.into_iter().map(str::to_owned)).map(|path| async move {
        let size = regular_file_size(&path).await;
        (path, size)
    }).buffered(8).collect().await;
    if let Some((path, Some(size))) = files.iter().find(|(_, size)| size.is_some_and(|size| size > MAX_IMAGE_BYTES)) {
        return Err(HtmlRenderError::ImageTooLarge { path: path.clone(), size_bytes: *size });
    }
    let sizes: HashMap<&str, Option<u64>> = files.iter().map(|(path, size)| (path.as_str(), *size)).collect();
    let page_bytes = references.iter().fold(html.len() as i64, |total, reference| match sizes.get(reference.path.as_str()).copied().flatten() {
        None => total,
        Some(size) => total + data_uri_prefix(&reference.path).len() as i64 + (size.div_ceil(3) * 4) as i64 - reference.path.len() as i64,
    });
    if page_bytes > MAX_PAGE_BYTES as i64 {
        return Err(HtmlRenderError::PageTooLarge { size_bytes: page_bytes as u64 });
    }
    // Files can grow after `stat`. Each read stops one byte past the image limit, and reading stops once the images read so far cannot fit the page.
    let readable: Vec<String> = files.iter().filter(|(_, size)| size.is_some()).map(|(path, _)| path.clone()).collect();
    let mut read_bytes = 0u64;
    let mut images: HashMap<String, Vec<u8>> = HashMap::new();
    let mut reads = stream::iter(readable).map(|path| async move { (read_image(&path).await, path) }).buffered(4);
    while let Some((read, path)) = reads.next().await {
        let Some(bytes) = read.filter(|bytes| is_image_bytes(bytes)) else { continue };
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(HtmlRenderError::ImageTooLarge { path, size_bytes: bytes.len() as u64 });
        }
        read_bytes += (bytes.len() as u64).div_ceil(3) * 4;
        if read_bytes > MAX_PAGE_BYTES {
            return Err(HtmlRenderError::PageTooLarge { size_bytes: read_bytes });
        }
        images.insert(path, bytes);
    }
    drop(reads);
    let data_uris: HashMap<&str, String> = images.iter().map(|(path, bytes)| (path.as_str(), data_uri_prefix(path) + &base64::engine::general_purpose::STANDARD.encode(bytes))).collect();
    let mut inlined = String::with_capacity(html.len());
    let mut cursor = 0;
    for reference in &references {
        let Some(data_uri) = data_uris.get(reference.path.as_str()) else { continue };
        inlined.push_str(&html[cursor..reference.start]);
        inlined.push_str(data_uri);
        cursor = reference.end;
    }
    inlined.push_str(&html[cursor..]);
    if inlined.len() as u64 > MAX_PAGE_BYTES {
        return Err(HtmlRenderError::PageTooLarge { size_bytes: inlined.len() as u64 });
    }
    let missing = files.iter().filter(|(path, _)| !data_uris.contains_key(path.as_str())).map(|(path, _)| path.clone()).collect();
    Ok(Inlined { html: inlined, missing })
}

const MIN_PREVIEW_WIDTH: i64 = 240;
const MAX_PREVIEW_WIDTH: i64 = 1_600;
// Each preview or measurement runs its own browser; more at once mostly costs memory.
const MAX_CONCURRENT_BROWSERS: usize = 2;
// Publishing waits this long at most for heights before returning without them.
const MEASURE_TIMEOUT: Duration = Duration::from_secs(6);

/// Where the headless shell comes from.
#[async_trait]
pub trait BrowserSource: Send + Sync {
    /// The installed headless shell, if any. Never starts or waits on an install.
    fn installed(&self) -> Option<PathBuf>;
    /// The installed headless shell, installing it if need be (see `PreviewBrowser::executable`).
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError>;
}

#[async_trait]
impl BrowserSource for PreviewBrowser {
    fn installed(&self) -> Option<PathBuf> {
        PreviewBrowser::installed(self)
    }
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError> {
        PreviewBrowser::executable(self).await
    }
}

pub struct HtmlRenderOptions {
    pub browser: Arc<dyn BrowserSource>,
    pub launcher: Arc<dyn BrowserLauncher>,
    /// Where published pages are stored as thread attachments.
    pub attachments_dir: PathBuf,
    /// Chrome's sandbox stays on unless the operator explicitly turns it off. Chrome also refuses it as root, where that opt-out is the only way to run.
    pub no_sandbox: bool,
    /// The command that sets the host up, named by every host setup error.
    pub setup_command: String,
}

/// A page checked in the browser.
#[derive(Debug, Clone, PartialEq)]
pub struct HtmlPreview {
    /// Base64 PNG of the top `captured_height` pixels.
    pub png: String,
    pub width: u32,
    /// Height the page needs to show without scrolling.
    pub content_height: u32,
    pub captured_height: u32,
    pub console_messages: Vec<ConsoleMessage>,
    /// Local image paths that could not be read; they show as broken images.
    pub missing_images: Option<Vec<String>>,
}

pub struct HtmlRender {
    options: HtmlRenderOptions,
    browsers: Semaphore,
}

/// The file of a page being published, removed unless the publish returns: only the returned reference lets thread deletion find the page, so
/// a publish that fails or is cancelled before returning removes it.
struct PublishedPage<'a> {
    path: &'a Path,
    kept: bool,
}
impl Drop for PublishedPage<'_> {
    fn drop(&mut self) {
        if !self.kept {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// JavaScript's `trim()` and `slice(0, max)`, which counts UTF-16 units.
fn trim_and_cut(text: &str, max_units: usize) -> String {
    let mut used = 0;
    text.trim_matches(is_js_space)
        .chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= max_units
        })
        .collect()
}

impl HtmlRender {
    pub fn new(options: HtmlRenderOptions) -> Self {
        Self { options, browsers: Semaphore::new(MAX_CONCURRENT_BROWSERS) }
    }

    /// The service of an installation: the pinned browser of `browser`, started as a process, storing pages under `attachments_dir`.
    pub fn production(browser: PreviewBrowser, attachments_dir: PathBuf) -> Self {
        Self::new(HtmlRenderOptions {
            browser: Arc::new(browser),
            launcher: Arc::new(ProcessLauncher),
            attachments_dir,
            no_sandbox: preview_browser_host::sandbox_disabled(|name| std::env::var(name).ok()),
            setup_command: root_setup_command(),
        })
    }

    /// Runs one browser launch, two at a time; a host that cannot start it gets setup steps instead.
    async fn launching<T>(&self, executable: &Path, run: impl std::future::Future<Output = Result<T, HtmlRenderBrowserError>>) -> Result<T, HtmlRenderError> {
        let _permit = self.browsers.acquire().await.expect("the semaphore is never closed");
        match run.await {
            Ok(value) => Ok(value),
            Err(error) => match error.output.as_deref() {
                None => Err(HtmlRenderError::Browser(error)),
                Some(output) => match preview_browser_host::diagnose_launch_failure(executable, output, &self.options.setup_command).await {
                    Some(host) => Err(HtmlRenderError::Host(host)),
                    None => Err(HtmlRenderError::Browser(error)),
                },
            },
        }
    }

    /// The heights the page needs at each client width, or none when the browser is not installed, cannot be started or takes too long. Never
    /// installs the browser: html_render must not depend on it.
    async fn measure(&self, html: &str) -> Option<Vec<(u32, u32)>> {
        let executable = self.options.browser.installed()?;
        let fragment = html_render_theme_fragment(&html_render_theme(&T3_CODE_DARK_THEME_COLORS, Appearance::Dark, HTML_RENDER_MEASURE_FONTS));
        let run = self.launching(&executable, headless_chrome::measure_html_heights(self.options.launcher.as_ref(), &executable, self.options.no_sandbox, html, &HTML_RENDER_MEASURE_WIDTHS, &fragment));
        let measured = match tokio::time::timeout(MEASURE_TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(HtmlRenderError::Browser(HtmlRenderBrowserError::new(format!("measuring took longer than {} seconds", MEASURE_TIMEOUT.as_secs())))),
        };
        match measured {
            Ok(mut heights) => {
                heights.sort_by_key(|(width, _)| *width);
                Some(heights)
            }
            Err(error) => {
                tracing::warn!(%error, "Could not measure an HTML render; publishing it without heights.");
                None
            }
        }
    }

    /// The bootstrap goes in first so its head scan never runs over inlined image data.
    async fn inline(&self, html: &str) -> Result<Inlined, HtmlRenderError> {
        inline_local_images(&inject_html_render_bootstrap(html)).await
    }

    /// The self-contained page that gets stored: local images inlined, theme bootstrap injected.
    pub async fn prepare(&self, html: &str) -> Result<String, HtmlRenderError> {
        let inlined = self.inline(html).await?;
        if !inlined.missing.is_empty() {
            return Err(HtmlRenderError::ImagesNotFound { paths: inlined.missing });
        }
        Ok(inlined.html)
    }

    /// Stores a prepared page as a thread attachment for clients to show inline, measuring its height at each client width when the preview browser
    /// is already installed. Returns the reference the tool answers with: `attachmentId`, `title`, `height` and, when measured, `heights`.
    pub async fn publish(&self, thread_id: &str, html: &str, title: &str, height: i64) -> Result<Value, HtmlRenderError> {
        let html = self.prepare(html).await?;
        // The `-html` id suffix makes the asset route serve it as a sandboxed text/html document.
        let attachment_id = attachment_store::create_attachment_id(thread_id, Some("html"));
        let path = attachment_id.as_ref().and_then(|id| attachment_store::resolve_relative_path(&self.options.attachments_dir, &format!("{id}.html")));
        let (Some(attachment_id), Some(path)) = (attachment_id, path) else { return Err(HtmlRenderError::Store { cause: "Invalid thread id.".into() }) };
        let store = |cause: std::io::Error| HtmlRenderError::Store { cause: cause.to_string() };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(store)?;
        }
        let mut page = PublishedPage { path: &path, kept: false };
        tokio::fs::write(&path, &html).await.map_err(store)?;
        let heights = self.measure(&html).await;
        page.kept = true;
        let mut reference = Map::new();
        reference.insert("attachmentId".into(), json!(attachment_id));
        reference.insert("title".into(), json!(match trim_and_cut(title, HTML_RENDER_MAX_TITLE_LENGTH) {
            title if title.is_empty() => "HTML".to_owned(),
            title => title,
        }));
        reference.insert("height".into(), json!(clamp_html_render_height(height as f64)));
        if let Some(heights) = heights {
            reference.insert("heights".into(), json!(heights.iter().map(|(width, height)| json!([width, height])).collect::<Vec<_>>()));
        }
        Ok(Value::Object(reference))
    }

    /// Screenshots a page in headless Chrome, tolerating unreadable local images.
    pub async fn preview(&self, html: &str, width: Option<i64>, appearance: Option<Appearance>) -> Result<HtmlPreview, HtmlRenderError> {
        let width = width.unwrap_or_else(|| i64::from(HTML_RENDER_COLUMN_WIDTH)).clamp(MIN_PREVIEW_WIDTH, MAX_PREVIEW_WIDTH) as u32;
        let appearance = appearance.unwrap_or(Appearance::Dark);
        let inlined = self.inline(html).await?;
        let executable = self.options.browser.executable().await.map_err(HtmlRenderError::Install)?;
        let fragment = html_render_theme_fragment(&html_render_theme(theme_colors(appearance), appearance, HTML_RENDER_MEASURE_FONTS));
        let input = headless_chrome::CaptureInput { executable: &executable, no_sandbox: self.options.no_sandbox, html: &inlined.html, width, url_fragment: &fragment };
        let capture = self.launching(&executable, headless_chrome::capture_html_screenshot(self.options.launcher.as_ref(), input)).await?;
        Ok(HtmlPreview {
            png: capture.png,
            width,
            content_height: capture.content_height,
            captured_height: capture.captured_height,
            console_messages: capture.console_messages,
            missing_images: (!inlined.missing.is_empty()).then_some(inlined.missing),
        })
    }
}

fn shell_word(value: &str) -> String {
    if !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// `t3 browser setup` as root, for the setup a person runs once on the host: through the absolute path of this binary, which `sudo` needs
/// no PATH for.
fn root_setup_command() -> String {
    let launcher = std::env::current_exe().ok().and_then(|path| path.to_str().map(shell_word)).unwrap_or_else(|| "t3".into());
    format!("sudo {launcher} {}", preview_browser_host::SETUP_SUBCOMMAND)
}

/// Gives `store` the service its HTML tools use, in place of the one that would install the pinned browser: for a test that must not reach the
/// network or start a browser. Has no effect once the store has made its own.
pub fn provide_for_store(store: &Store, service: HtmlRender) {
    store.extension(|| Some(Arc::new(service)));
}

/// The pinned browser of the installation `store` belongs to, made on first use. None for the in-memory store, which has no home to install under.
pub fn preview_browser_for(store: &Store) -> Option<PreviewBrowser> {
    let made = store.extension(|| store.state_dir().map(|state_dir| PreviewBrowser::new(PreviewBrowserOptions::new(crate::connect::base_dir_of(state_dir)))));
    (*made).clone()
}

/// Stops an install of the pinned browser that is still running, as the end of the server does. A browser an HTML render started ends with the
/// render that holds it.
pub fn shutdown(store: &Store) {
    if let Some(browser) = preview_browser_for(store) {
        browser.shutdown();
    }
}

/// The HTML render service of the installation `store` belongs to, made on first use. None for the in-memory store, which has nowhere to keep pages.
pub fn for_store(store: &Store) -> Option<Arc<HtmlRender>> {
    // The browser is made before the store's extensions are locked for this one.
    let browser = preview_browser_for(store);
    let made = store.extension(|| {
        let attachments = crate::attachment_service::AttachmentService::for_store(store)?;
        Some(Arc::new(HtmlRender::production(browser?, attachments.attachments_dir().to_owned())))
    });
    (*made).clone()
}

