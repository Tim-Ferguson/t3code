//! The HTML render service: inlining a page's local images, storing a page, measuring it, and previewing it, against a scripted browser. The
//! image matcher and recogniser are checked against what the original's own code answers for a corpus (`html-render-images.jsonl`, written by
//! `tools/generate_html_render_fixtures.mjs`). No browser is started or downloaded.
use crate::{
    cdp_fake::{FakeBrowser, FakeLauncher, Pages, measured, page_handler},
    headless_chrome::{BrowserHandle, BrowserLauncher, HtmlRenderBrowserError, LaunchInput, LaunchedBrowser},
    html_render::*,
    html_render_shared::{Appearance, HTML_RENDER_MEASURE_FONTS, HTML_RENDER_MEASURE_WIDTHS, html_render_theme, html_render_theme_fragment, theme_colors},
    preview_browser_host::PreviewBrowserHostError,
    preview_browser_install::PreviewBrowserError,
    test_support::HANG_BOUND,
};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

fn rows(kind: &str) -> Vec<Value> {
    include_str!("../tests/fixtures/html-render-images.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == kind)
        .collect()
}

#[test]
fn local_images_are_found_as_the_originals_pattern_finds_them() {
    let cases = rows("find");
    assert!(cases.len() >= 40, "{} cases", cases.len());
    for case in cases {
        let html = case["html"].as_str().unwrap();
        let found = find_local_images(html);
        let paths: Vec<&str> = found.iter().map(|reference| reference.path.as_str()).collect();
        let expected: Vec<&str> = case["paths"].as_array().unwrap().iter().map(|path| path.as_str().unwrap()).collect();
        assert_eq!(paths, expected, "{html:?}");
        // The spans line up with the text too: replacing each by its path in marks gives the original's own marking.
        let mut marked = String::new();
        let mut cursor = 0;
        for reference in &found {
            marked.push_str(&html[cursor..reference.start]);
            marked.push_str(&format!("<<{}>>", reference.path));
            cursor = reference.end;
        }
        marked.push_str(&html[cursor..]);
        assert_eq!(marked, case["marked"].as_str().unwrap(), "{html:?}");
    }
}

#[test]
fn file_bytes_are_recognised_as_images_as_the_original_recognises_them() {
    let cases = rows("image");
    assert!(cases.len() >= 60, "{} cases", cases.len());
    for case in cases {
        let bytes = base64::engine::general_purpose::STANDARD.decode(case["bytes"].as_str().unwrap()).unwrap();
        assert_eq!(is_image_bytes(&bytes), case["image"].as_bool().unwrap(), "{:?}", String::from_utf8_lossy(&bytes[..bytes.len().min(80)]));
    }
}

#[test]
fn mime_types_and_windows_escapes_follow_the_original() {
    for case in rows("mime") {
        assert_eq!(data_uri_prefix(case["path"].as_str().unwrap()), case["prefix"].as_str().unwrap(), "{}", case["path"]);
    }
    let cases = rows("file");
    assert!(!cases.is_empty());
    for case in cases {
        assert_eq!(file_path_for(case["path"].as_str().unwrap()), case["file"].as_str().unwrap(), "{}", case["path"]);
    }
}

/// Where the browser comes from in these tests.
struct Source {
    installed: Option<PathBuf>,
    executable: Result<PathBuf, PreviewBrowserError>,
    asked_to_install: AtomicUsize,
}
impl Source {
    fn installed() -> Arc<Self> {
        Arc::new(Self { installed: Some("/fake/chrome-headless-shell".into()), executable: Ok("/fake/chrome-headless-shell".into()), asked_to_install: AtomicUsize::new(0) })
    }
    fn not_installed() -> Arc<Self> {
        Arc::new(Self { installed: None, executable: Err(PreviewBrowserError::Installing { downloaded_bytes: 50_000_000, total_bytes: 99_221_129, unpacking: false }), asked_to_install: AtomicUsize::new(0) })
    }
}
#[async_trait]
impl BrowserSource for Source {
    fn installed(&self) -> Option<PathBuf> {
        self.installed.clone()
    }
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError> {
        self.asked_to_install.fetch_add(1, Ordering::SeqCst);
        self.executable.clone()
    }
}

fn service(attachments: &Path, source: Arc<Source>, launcher: Arc<dyn BrowserLauncher>) -> HtmlRender {
    HtmlRender::new(HtmlRenderOptions { browser: source, launcher, attachments_dir: attachments.to_owned(), no_sandbox: false, setup_command: "sudo t3 browser setup".into() })
}

/// A service that has no browser to launch, over a fresh attachments directory.
fn without_browser() -> (HtmlRender, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let launcher: Arc<dyn BrowserLauncher> = Arc::new(FakeLauncher::new(FakeBrowser::with_handler(|_| Vec::new())));
    (service(&directory.path().join("attachments"), Source::not_installed(), launcher), directory)
}

const PNG_BYTES: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

fn base64_of(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn local_images_are_inlined_by_absolute_path_and_urls_and_relative_paths_are_left_alone() {
    let (render, directory) = without_browser();
    let png = directory.path().join("shot.png");
    let svg = directory.path().join("logo.svg");
    std::fs::write(&png, PNG_BYTES).unwrap();
    std::fs::write(&svg, "<svg/>").unwrap();
    let kept = ["https://example.com/a.png", "//cdn.example.com/b.png", "./c.png", "data:image/png;base64,AAAA"];
    let html = [
        "<!doctype html><html><head><title>Shots</title></head><body>".to_owned(),
        format!("<img src=\"{}\"><div style=\"background:url({})\"></div>", png.display(), svg.display()),
        format!("<script>const shots = ['{}', `{}`];</script>", png.display(), svg.display()),
        kept.iter().map(|src| format!("<img src=\"{src}\">")).collect::<String>(),
        "</body></html>".to_owned(),
    ]
    .concat();
    let prepared = render.prepare(&html).await.unwrap();
    let png_uri = format!("data:image/png;base64,{}", base64_of(&PNG_BYTES));
    let svg_uri = format!("data:image/svg+xml;base64,{}", base64_of(b"<svg/>"));
    assert!(prepared.contains(&format!("<img src=\"{png_uri}\">")));
    assert!(prepared.contains(&format!("url({svg_uri})")));
    assert!(prepared.contains(&format!("['{png_uri}', `{svg_uri}`]")));
    assert!(!prepared.contains(directory.path().to_str().unwrap()));
    for src in kept {
        assert!(prepared.contains(&format!("<img src=\"{src}\">")), "{src}");
    }
    // The theme bootstrap opens the head, ahead of the page's own markup.
    let theme = prepared.find("<style id=\"t3-theme\">").unwrap();
    assert!(prepared.find("<head>").unwrap() < theme && theme < prepared.find("<title>").unwrap());
}

#[tokio::test]
async fn an_svg_behind_processing_instructions_and_a_doctype_subset_is_inlined() {
    let (render, directory) = without_browser();
    let svg = directory.path().join("styled.svg");
    let source = ["<?xml version=\"1.0\"?>", "<?xml-stylesheet href=\"theme.css\"?>", "<!DOCTYPE svg [ <!ENTITY fill \"red\"> ]>", "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"32\" height=\"32\"/>"].join("\n");
    std::fs::write(&svg, &source).unwrap();
    let prepared = render.prepare(&format!("<img src=\"{}\">", svg.display())).await.unwrap();
    assert!(prepared.contains(&format!("data:image/svg+xml;base64,{}", base64_of(source.as_bytes()))));
}

#[tokio::test]
async fn every_image_that_cannot_be_read_is_listed_and_nothing_that_is_not_an_image_is_inlined() {
    let (render, directory) = without_browser();
    let path = |name: &str| directory.path().join(name);
    let folder = path("folder.png");
    std::fs::create_dir(&folder).unwrap();
    let missing = path("missing.jpg");
    // Named like an image, but a symlink or renamed file must not carry other data.
    let write = |name: &str, content: &str| {
        std::fs::write(path(name), content).unwrap();
        path(name)
    };
    let secret = write("secret.png", "API_KEY=abc123");
    let report = write("report.svg", "<!doctype html><body><svg></svg>API_KEY=abc123");
    // An <svg> inside a quoted entity, and a prolog shaped to stall a backtracking matcher.
    let config = write("config.svg", "<!DOCTYPE config [<!ENTITY a \"a\"><!ENTITY b \"]><svg/>\">]><config>API_KEY=abc123</config>");
    let stalling = write("stalling.svg", &format!("{}<config><svg/></config>", "<?p?>".repeat(40)));
    let unclosed = write("unclosed.svg", "<!DOCTYPE svg [<!ENTITY a \"x><svg/>");
    // Roots named SVG or svgé are other elements.
    let upper = write("upper.svg", "<SVG/>API_KEY=abc123");
    let longer = write("longer.svg", "<svg\u{e9}/>API_KEY=abc123");
    let html = format!(
        "<img src=\"{}\"><img src='{}'><img src=\"C:\\nope\\shot.webp\"><img src=\"{}\"><img src=\"{}\"><img src=\"{}\"><img src=\"{}\"><img src=\"{}\"><img src=\"{}\"><img src=\"{}\">",
        missing.display(),
        folder.display(),
        secret.display(),
        report.display(),
        config.display(),
        stalling.display(),
        unclosed.display(),
        upper.display(),
        longer.display()
    );
    let error = render.prepare(&html).await.unwrap_err();
    let paths: Vec<String> = [&missing, &folder].iter().map(|path| path.display().to_string()).chain(["C:\\nope\\shot.webp".to_owned()]).chain([&secret, &report, &config, &stalling, &unclosed, &upper, &longer].iter().map(|path| path.display().to_string())).collect();
    assert_eq!(error, HtmlRenderError::ImagesNotFound { paths: paths.clone() });
    assert_eq!(error.message(), format!("These local images could not be read: {}. Use absolute paths to existing image files, or remove them.", paths.join(", ")));
    assert!(error.invalid_page());
}

#[tokio::test]
async fn an_image_or_page_over_the_limits_is_refused_with_its_size() {
    let (render, directory) = without_browser();
    let sparse = |name: &str, bytes: u64| {
        let path = directory.path().join(name);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(bytes).unwrap();
        path
    };
    let big = sparse("big.png", 10 * 1024 * 1024 + 1);
    let error = render.prepare(&format!("<img src=\"{}\">", big.display())).await.unwrap_err();
    assert_eq!(error, HtmlRenderError::ImageTooLarge { path: big.display().to_string(), size_bytes: 10 * 1024 * 1024 + 1 });
    assert_eq!(error.message(), format!("{} is 10.0 MiB; each local image must be at most 10.0 MiB.", big.display()));
    // Three images that are each fine but are more than the page may hold once encoded.
    let parts: Vec<_> = (0..3).map(|n| sparse(&format!("part{n}.png"), 9 * 1024 * 1024)).collect();
    let html: String = parts.iter().map(|path| format!("<img src=\"{}\">", path.display())).collect();
    let error = render.prepare(&html).await.unwrap_err();
    let HtmlRenderError::PageTooLarge { size_bytes } = error else { panic!("{error:?}") };
    assert!(size_bytes > 25 * 1024 * 1024 && size_bytes < 40 * 1024 * 1024, "{size_bytes}");
    assert!(error.message().starts_with("With its images inlined the page is 3") && error.message().ends_with("MiB; the limit is 25.0 MiB. Use smaller images."), "{}", error.message());
    assert!(error.invalid_page());
}

#[test]
fn sizes_are_written_in_tenths_of_a_mebibyte_rounded_half_up() {
    let message = |bytes: u64| HtmlRenderError::PageTooLarge { size_bytes: bytes }.message();
    // 0.25 MiB is exact, and a tie goes up, as `toFixed(1)` does.
    assert!(message(262_144).contains("page is 0.3 MiB"), "{}", message(262_144));
    assert!(message(1_048_576).contains("page is 1.0 MiB"));
    assert!(message(10 * 1_048_576 + 52_428).contains("page is 10.0 MiB"));
    assert!(message(10 * 1_048_576 + 52_429).contains("page is 10.1 MiB"));
    assert!(message(0).contains("page is 0.0 MiB"));
}

#[test]
fn every_error_says_what_it_is_and_what_to_do() {
    let cases: Vec<(HtmlRenderError, &str, &str, bool)> = vec![
        (HtmlRenderError::Store { cause: "disk full".into() }, "HtmlRenderStoreError", "The HTML render could not be saved.", false),
        (HtmlRenderError::Install(PreviewBrowserError::Installing { downloaded_bytes: 50_000_000, total_bytes: 99_221_129, unpacking: false }), "PreviewBrowserInstallingError", "T3 is installing its headless browser (50 of 99 MB downloaded). Try again in a minute.", false),
        (HtmlRenderError::Install(PreviewBrowserError::Unsupported { platform: "freebsd".into(), arch: "x64".into() }), "PreviewBrowserUnsupportedError", "T3's headless browser is not available on freebsd-x64: Chrome for Testing has no headless shell for it.", false),
        (HtmlRenderError::Host(PreviewBrowserHostError::Sandbox { setup_command: "sudo t3 browser setup".into() }), "PreviewBrowserSandboxError", "This host blocks the sandbox T3's browser runs in (AppArmor on Ubuntu 23.10+). Run `sudo t3 browser setup` on the host once to allow it, then try again.", false),
        (HtmlRenderError::Browser(HtmlRenderBrowserError::new("the browser exited unexpectedly")), "HtmlRenderBrowserError", "Headless Chrome could not render the page: the browser exited unexpectedly.", false),
    ];
    for (error, tag, message, invalid) in cases {
        assert_eq!((error.tag(), error.message().as_str(), error.invalid_page()), (tag, message, invalid));
    }
}

fn stored_pages(attachments: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(attachments) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).filter(|path| path.extension().is_some_and(|extension| extension == "html")).collect(),
        Err(_) => Vec::new(),
    }
}

#[tokio::test]
async fn a_page_is_stored_as_an_html_thread_attachment_and_published_unmeasured_without_an_installed_browser() {
    let (render, directory) = without_browser();
    let attachments = directory.path().join("attachments");
    let reference = render.publish("thread-html-render", "<p>Quarterly revenue</p>", "  Revenue  ", 9_000).await.unwrap();
    let id = reference["attachmentId"].as_str().unwrap();
    assert_eq!(reference, json!({"attachmentId": id, "title": "Revenue", "height": 2000}));
    assert!(id.starts_with("thread-html-render-") && id.ends_with("-html"), "{id}");
    let stored = std::fs::read_to_string(attachments.join(format!("{id}.html"))).unwrap();
    assert!(stored.contains("<style id=\"t3-theme\">") && stored.contains("<p>Quarterly revenue</p>"));
    assert_eq!(stored_pages(&attachments).len(), 1);
}

#[tokio::test]
async fn titles_are_trimmed_cut_in_utf16_units_and_never_empty() {
    let (render, _directory) = without_browser();
    let title = |title: &str| {
        let render = &render;
        let title = title.to_owned();
        async move { render.publish("t", "<p>x</p>", &title, 100).await.unwrap()["title"].as_str().unwrap().to_owned() }
    };
    assert_eq!(title("\u{a0}\u{feff} Chart \n").await, "Chart");
    assert_eq!(title("   ").await, "HTML");
    assert_eq!(title(&"a".repeat(250)).await, "a".repeat(200));
    // 200 units hold a hundred emoji, and a cut never splits one.
    assert_eq!(title(&"\u{1F600}".repeat(150)).await, "\u{1F600}".repeat(100));
    assert_eq!(title(&format!("a{}", "\u{1F600}".repeat(150))).await, format!("a{}", "\u{1F600}".repeat(99)));
}

#[tokio::test]
async fn heights_are_clamped_to_the_frame_limits() {
    let (render, _directory) = without_browser();
    for (asked, kept) in [(-10, 80), (0, 80), (79, 80), (80, 80), (700, 700), (2000, 2000), (2001, 2000), (1_000_000, 2000)] {
        assert_eq!(render.publish("t", "<p>x</p>", "T", asked).await.unwrap()["height"], kept);
    }
}

#[tokio::test]
async fn a_thread_that_names_no_storage_is_a_store_error_and_nothing_is_written() {
    let (render, directory) = without_browser();
    for thread in ["", "   ", "---"] {
        let error = render.publish(thread, "<p>x</p>", "T", 100).await.unwrap_err();
        assert_eq!(error, HtmlRenderError::Store { cause: "Invalid thread id.".into() }, "{thread:?}");
    }
    assert!(stored_pages(&directory.path().join("attachments")).is_empty());
}

#[tokio::test]
async fn a_missing_image_stops_a_publish_before_anything_is_stored() {
    let (render, directory) = without_browser();
    let error = render.publish("t", "<img src=\"/nonexistent/t3-missing.png\">", "T", 100).await.unwrap_err();
    assert_eq!(error, HtmlRenderError::ImagesNotFound { paths: vec!["/nonexistent/t3-missing.png".into()] });
    assert!(stored_pages(&directory.path().join("attachments")).is_empty());
}

#[tokio::test]
async fn a_published_page_is_measured_at_every_client_width_when_the_browser_is_installed() {
    let directory = tempfile::tempdir().unwrap();
    let pages = Arc::new(Mutex::new(Pages::default()));
    // Like a D3 chart, the page sizes itself from the width once, at load.
    let browser = FakeBrowser::with_handler(page_handler(pages.clone(), |width| measured(f64::from(width) / 2.0), Vec::new));
    let launcher = Arc::new(FakeLauncher::new(browser));
    let source = Source::installed();
    let render = service(&directory.path().join("attachments"), source.clone(), launcher.clone());
    let reference = render.publish("thread-html-measure", "<div id=\"chart\"></div>", "Chart", 600).await.unwrap();
    let expected: Vec<Value> = HTML_RENDER_MEASURE_WIDTHS.iter().map(|width| json!([width, width.div_ceil(2)])).collect();
    assert_eq!(reference["heights"], Value::Array(expected));
    assert_eq!((reference["title"].as_str(), reference["height"].as_i64()), (Some("Chart"), Some(600)));
    assert_eq!(pages.lock().unwrap().navigations, 9, "a fresh load for each width");
    // Measuring never installs the browser.
    assert_eq!(source.asked_to_install.load(Ordering::SeqCst), 0);
    // The page is laid out in the dark theme with Arial-metric fonts.
    let fragment = html_render_theme_fragment(&html_render_theme(theme_colors(Appearance::Dark), Appearance::Dark, HTML_RENDER_MEASURE_FONTS));
    let navigated = launcher.browser.commands_named("Page.navigate");
    assert!(navigated.iter().all(|command| command.params["url"] == format!("http://t3-page.localhost/page.html{fragment}")));
}

#[tokio::test]
async fn a_publish_whose_measurement_fails_still_stores_the_page_without_heights() {
    let directory = tempfile::tempdir().unwrap();
    let attachments = directory.path().join("attachments");
    // The browser goes away as soon as it is asked for a page.
    let browser = FakeBrowser::with_handler(|command| if command.method == "Target.createTarget" { vec![json!({"$hangUp": true})] } else { Vec::new() });
    let render = service(&attachments, Source::installed(), Arc::new(FakeLauncher::new(browser)));
    let reference = render.publish("thread-html-fail", "<p>x</p>", "T", 300).await.unwrap();
    assert_eq!(reference.as_object().unwrap().keys().collect::<Vec<_>>(), ["attachmentId", "title", "height"]);
    assert_eq!(stored_pages(&attachments).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_measurement_that_takes_more_than_six_seconds_is_given_up_on() {
    let directory = tempfile::tempdir().unwrap();
    let attachments = directory.path().join("attachments");
    let browser = FakeBrowser::with_handler(|_| Vec::new());
    let render = service(&attachments, Source::installed(), Arc::new(FakeLauncher::new(browser)));
    let started = tokio::time::Instant::now();
    let reference = render.publish("thread-html-slow", "<p>x</p>", "T", 300).await.unwrap();
    assert!(reference.get("heights").is_none());
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(6));
    assert_eq!(stored_pages(&attachments).len(), 1);
}

#[tokio::test]
async fn a_publish_that_is_cancelled_while_measuring_removes_the_page() {
    let directory = tempfile::tempdir().unwrap();
    let attachments = directory.path().join("attachments");
    let (asked, asked_for_a_page) = tokio::sync::oneshot::channel();
    let asked = Mutex::new(Some(asked));
    // The browser is asked for its first page and never answers.
    let browser = FakeBrowser::with_handler(move |command| {
        if command.method == "Target.createTarget" {
            if let Some(asked) = asked.lock().unwrap().take() {
                let _ = asked.send(());
            }
        }
        Vec::new()
    });
    let render = Arc::new(service(&attachments, Source::installed(), Arc::new(FakeLauncher::new(browser))));
    let publishing = {
        let render = render.clone();
        tokio::spawn(async move { render.publish("thread-html-cancel", "<p>x</p>", "X", 200).await })
    };
    within(asked_for_a_page).await.unwrap();
    assert_eq!(stored_pages(&attachments).len(), 1, "the page is stored while it is measured");
    publishing.abort();
    assert!(within(publishing).await.unwrap_err().is_cancelled());
    assert!(stored_pages(&attachments).is_empty(), "nothing can find the page, so it is removed");
}

const CAPTURE: &str = include_str!("../tests/fixtures/cdp-html-capture.jsonl");

fn previewing(source: Arc<Source>) -> (HtmlRender, Arc<FakeLauncher>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new(FakeBrowser::replay(CAPTURE)));
    (service(&directory.path().join("attachments"), source, launcher.clone()), launcher, directory)
}

#[tokio::test]
async fn a_preview_is_a_screenshot_with_the_height_and_what_the_page_logged() {
    let (render, launcher, _directory) = previewing(Source::installed());
    let preview = within(render.preview("<p>x</p>", None, None)).await.unwrap();
    assert_eq!((preview.png.as_str(), preview.width, preview.content_height, preview.captured_height), ("iVBORw0KGgo=", 728, 301, 301));
    assert_eq!(preview.console_messages.len(), 5);
    assert_eq!(preview.missing_images, None);
    // Dark, the reply column's width, Arial-metric fonts.
    let fragment = html_render_theme_fragment(&html_render_theme(theme_colors(Appearance::Dark), Appearance::Dark, HTML_RENDER_MEASURE_FONTS));
    assert_eq!(launcher.browser.commands_named("Page.navigate")[0].params["url"], format!("http://t3-page.localhost/page.html{fragment}"));
    assert_eq!(launcher.browser.commands_named("Emulation.setDeviceMetricsOverride")[0].params["width"], 728);
}

#[tokio::test]
async fn a_previews_width_is_kept_between_240_and_1600_and_its_theme_is_the_one_asked_for() {
    for (asked, used) in [(Some(100), 240), (Some(239), 240), (Some(240), 240), (Some(390), 390), (Some(1600), 1600), (Some(5000), 1600), (Some(-4), 240)] {
        let (render, launcher, _directory) = previewing(Source::installed());
        let preview = within(render.preview("<p>x</p>", asked, Some(Appearance::Light))).await.unwrap();
        assert_eq!(preview.width, used);
        assert_eq!(launcher.browser.commands_named("Emulation.setDeviceMetricsOverride")[0].params["width"], used);
        assert_eq!(launcher.browser.commands_named("Page.captureScreenshot")[0].params["clip"]["width"], used);
        let fragment = html_render_theme_fragment(&html_render_theme(theme_colors(Appearance::Light), Appearance::Light, HTML_RENDER_MEASURE_FONTS));
        assert_eq!(launcher.browser.commands_named("Page.navigate")[0].params["url"], format!("http://t3-page.localhost/page.html{fragment}"));
    }
}

#[tokio::test]
async fn a_preview_serves_the_page_with_its_images_inlined_and_reports_the_ones_it_could_not_read() {
    let (render, launcher, directory) = previewing(Source::installed());
    let png = directory.path().join("shot.png");
    std::fs::write(&png, PNG_BYTES).unwrap();
    let preview = within(render.preview(&format!("<img src=\"{}\"><img src=\"/nonexistent/t3-missing.png\" hidden>", png.display()), None, None)).await.unwrap();
    // The unreadable one is reported and stays in the page as written; the preview does not fail for it.
    assert_eq!(preview.missing_images, Some(vec!["/nonexistent/t3-missing.png".to_owned()]));
    let body = launcher.browser.commands_named("Fetch.fulfillRequest")[0].params["body"].as_str().unwrap().to_owned();
    let served = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(body).unwrap()).unwrap();
    assert!(served.contains(&format!("<img src=\"data:image/png;base64,{}\">", base64_of(&PNG_BYTES))));
    assert!(served.contains("<img src=\"/nonexistent/t3-missing.png\" hidden>") && served.contains("<style id=\"t3-theme\">"));
}

#[tokio::test]
async fn a_preview_on_a_machine_still_installing_the_browser_says_so() {
    let source = Source::not_installed();
    let (render, launcher, _directory) = previewing(source.clone());
    let error = render.preview("<p>x</p>", None, None).await.unwrap_err();
    assert_eq!(error.tag(), "PreviewBrowserInstallingError");
    assert_eq!(error.message(), "T3 is installing its headless browser (50 of 99 MB downloaded). Try again in a minute.");
    assert_eq!(source.asked_to_install.load(Ordering::SeqCst), 1);
    assert!(launcher.launches().is_empty());
}

#[tokio::test]
async fn a_page_with_a_missing_image_is_previewed_not_refused() {
    let (render, _launcher, _directory) = previewing(Source::installed());
    assert!(within(render.preview("<img src=\"/nonexistent/a.png\">", None, None)).await.is_ok());
}

fn hanging_up_on_navigation() -> FakeBrowser {
    FakeBrowser::with_handler(|command| match command.method.as_str() {
        "Target.createTarget" => vec![json!({"id": command.id, "result": {"targetId": "T-1"}})],
        "Target.attachToTarget" => vec![json!({"id": command.id, "result": {"sessionId": "S-1"}})],
        "Page.navigate" => vec![json!({"$hangUp": true})],
        _ => vec![json!({"id": command.id, "sessionId": command.session_id, "result": {}})],
    })
}

#[tokio::test]
async fn a_browser_that_cannot_run_for_want_of_a_sandbox_is_blamed_on_the_host_with_the_command_that_fixes_it() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new(hanging_up_on_navigation()).exiting_with("[1:1:ERROR:zygote_host_impl_linux.cc(90)] No usable sandbox! Update your kernel"));
    let render = service(&directory.path().join("attachments"), Source::installed(), launcher);
    let error = within(render.preview("<p>x</p>", None, None)).await.unwrap_err();
    assert_eq!(error, HtmlRenderError::Host(PreviewBrowserHostError::Sandbox { setup_command: "sudo t3 browser setup".into() }));
    assert_eq!(error.tag(), "PreviewBrowserSandboxError");
}

#[tokio::test]
async fn a_browser_that_dies_for_another_reason_is_reported_as_having_exited() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new(hanging_up_on_navigation()).exiting_with("segmentation fault"));
    let render = service(&directory.path().join("attachments"), Source::installed(), launcher);
    let error = within(render.preview("<p>x</p>", None, None)).await.unwrap_err();
    assert_eq!(error.message(), "Headless Chrome could not render the page: the browser exited unexpectedly.");
    assert!(matches!(error, HtmlRenderError::Browser(HtmlRenderBrowserError { output: Some(output), .. }) if output == "segmentation fault"));
}

/// Starts browsers that never answer, counting how many are running at once.
struct Counting {
    running: Arc<AtomicUsize>,
    most: Arc<AtomicUsize>,
    started: AtomicUsize,
}
struct Running(Arc<AtomicUsize>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl BrowserHandle for Running {
    async fn exited(&self) -> HtmlRenderBrowserError {
        HtmlRenderBrowserError::new("the browser exited unexpectedly")
    }
    async fn shutdown(&self) {}
}
#[async_trait]
impl BrowserLauncher for Counting {
    async fn launch(&self, _: &LaunchInput) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        let (reader, writer) = FakeBrowser::with_handler(|_| Vec::new()).pipe();
        Ok(LaunchedBrowser { connection: crate::cdp::CdpConnection::over_pipe(reader, writer), handle: Arc::new(Running(self.running.clone())) })
    }
}

#[tokio::test(start_paused = true)]
async fn no_more_than_two_browsers_run_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let launcher = Arc::new(Counting { running: Arc::default(), most: Arc::default(), started: AtomicUsize::new(0) });
    let render = service(&directory.path().join("attachments"), Source::installed(), launcher.clone());
    // Each hangs until its capture times out; the third waits for one of the others to be done.
    let previews = futures_util::future::join_all((0..5).map(|_| render.preview("<p>x</p>", None, None))).await;
    assert!(previews.iter().all(|preview| matches!(preview, Err(HtmlRenderError::Browser(error)) if error.reason == "the page did not finish loading within 20 seconds")));
    assert_eq!(launcher.started.load(Ordering::SeqCst), 5);
    assert_eq!(launcher.most.load(Ordering::SeqCst), 2);
}
