//! Rendering a page against a scripted browser (recorded frames of the protocol, `cdp-html-capture.jsonl`, and handlers that play a page), and the
//! process launcher against shell scripts that stand in for the browser. No Chrome is started or downloaded.
use crate::cdp_fake::{FakeBrowser, FakeLauncher, Pages, measured, page_handler};
use crate::headless_chrome::*;
use crate::test_support::HANG_BOUND;
use base64::Engine;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

const FRAGMENT: &str = "#t3-theme=%7B%22appearance%22%3A%22dark%22%7D";
const PAGE_URL: &str = "http://t3-page.localhost/page.html";

fn capture_input<'a>(html: &'a str, width: u32) -> CaptureInput<'a> {
    CaptureInput { executable: Path::new("/fake/chrome-headless-shell"), no_sandbox: false, html, width, url_fragment: FRAGMENT }
}

fn methods(browser: &FakeBrowser) -> Vec<String> {
    browser.commands().into_iter().map(|command| command.method).collect()
}

#[tokio::test]
async fn a_page_is_served_loaded_measured_and_captured_from_the_recorded_frames() {
    let launcher = FakeLauncher::new(FakeBrowser::replay(include_str!("../tests/fixtures/cdp-html-capture.jsonl")));
    let html = "<!doctype html><p>caf\u{e9}</p>";
    let capture = within(capture_html_screenshot(&launcher, capture_input(html, 400))).await.unwrap();

    // 300.2 rounds up to the next pixel; the screenshot is the data the browser returned.
    assert_eq!((capture.png.as_str(), capture.content_height, capture.captured_height), ("iVBORw0KGgo=", 301, 301));
    let texts: Vec<(&str, &str)> = capture.console_messages.iter().map(|message| (message.level.as_str(), message.text.as_str())).collect();
    assert_eq!(
        texts,
        [
            ("log", "ready 3"),
            ("info", "\"Liberation Sans\", Arimo, Arial, Helvetica, sans-serif"),
            // A string, a boolean, null, an undefined value (shown as its type) and an object (shown as its description).
            ("warning", "#0a0a0a true null undefined Object"),
            // The page's URL is shown as the page's name.
            ("error", "Error: broken chart\n    at page.html:1:7"),
            ("error", "Failed to load resource: net::ERR_FAILED http://t3-page.localhost/missing.png"),
        ]
    );

    // Every command, in the order sent: the page is set up, navigated, and every request it paused is answered before it loads.
    let browser = &launcher.browser;
    assert_eq!(
        methods(browser),
        [
            "Target.createTarget",
            "Target.attachToTarget",
            "Page.enable",
            "Runtime.enable",
            "Log.enable",
            "Page.addScriptToEvaluateOnNewDocument",
            "Fetch.enable",
            "Emulation.setDeviceMetricsOverride",
            "Page.navigate",
            "Fetch.failRequest",
            "Fetch.continueRequest",
            "Fetch.failRequest",
            "Fetch.failRequest",
            "Fetch.fulfillRequest",
            "Runtime.evaluate",
            "Runtime.evaluate",
            "Page.captureScreenshot",
        ]
    );
    let sent = browser.commands();
    let param = |at: usize| sent[at].params.clone();
    assert_eq!(param(0), json!({"url": "about:blank"}));
    assert_eq!(param(1), json!({"targetId": "TARGET-1", "flatten": true}));
    assert_eq!(sent[2].session_id.as_deref(), Some("SESSION-1"));
    assert_eq!(param(5), json!({"source": "delete window.RTCPeerConnection; delete window.webkitRTCPeerConnection;", "runImmediately": true}));
    assert_eq!(param(6), json!({"patterns": [{"urlPattern": "http://t3-page.localhost/*"}, {"urlPattern": "*", "resourceType": "Document"}]}));
    assert_eq!(param(7), json!({"width": 400, "height": 800, "deviceScaleFactor": 1, "mobile": false}));
    assert_eq!(param(8), json!({"url": format!("{PAGE_URL}{FRAGMENT}")}));
    // A script from a file and a navigation of the main frame are refused, and so is a frame at the page's own origin; a frame on another site
    // is let through, since the proxy decides what it can reach.
    assert_eq!(param(9), json!({"requestId": "interception-job-1.0", "errorReason": "AccessDenied"}));
    assert_eq!(param(10), json!({"requestId": "interception-job-2.0"}));
    assert_eq!(param(11), json!({"requestId": "interception-job-3.0", "errorReason": "AccessDenied"}));
    assert_eq!(param(12), json!({"requestId": "interception-job-4.0", "errorReason": "AccessDenied"}));
    // The page itself is served from its bytes, as base64.
    let served = &sent[13];
    assert_eq!(served.session_id.as_deref(), Some("SESSION-1"));
    assert_eq!(
        served.params,
        json!({
            "requestId": "interception-job-5.0", "responseCode": 200,
            "responseHeaders": [{"name": "Content-Type", "value": "text/html; charset=utf-8"}],
            "body": base64::engine::general_purpose::STANDARD.encode(html.as_bytes()),
        })
    );
    assert_eq!(param(14)["awaitPromise"], true);
    assert!(param(14)["expression"].as_str().unwrap().contains("document.fonts.ready"));
    assert_eq!(param(15)["returnByValue"], true);
    assert_eq!(param(16), json!({"format": "png", "captureBeyondViewport": true, "clip": {"x": 0, "y": 0, "width": 400, "height": 301, "scale": 1}}));

    // The browser was started once, in a scratch profile that is gone, and ended.
    let launches = launcher.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].0, Path::new("/fake/chrome-headless-shell"));
    assert!(!launches[0].1);
    assert!(launches[0].2.ends_with("profile") && launches[0].2.parent().unwrap().file_name().unwrap().to_str().unwrap().starts_with("t3-html-preview-"));
    assert!(!launches[0].2.parent().unwrap().exists(), "the scratch directory is removed");
    assert_eq!(launcher.shutdowns(), 1);
}

async fn capture_with_height(value: f64) -> Capture {
    let browser = FakeBrowser::with_handler(page_handler(Arc::default(), move |_| measured(value), Vec::new));
    let launcher = FakeLauncher::new(browser);
    within(capture_html_screenshot(&launcher, capture_input("<p>x</p>", 500))).await.unwrap()
}

#[tokio::test]
async fn the_capture_is_the_pages_height_up_to_a_cap_and_at_least_one_pixel() {
    for (measured, content, captured) in [(300.0, 300, 300), (300.0001, 301, 301), (3999.5, 4000, 4000), (4000.0, 4000, 4000), (4001.0, 4001, 4000), (9000.0, 9000, 4000), (0.0, 0, 1), (-5.5, 0, 1), (0.2, 1, 1)] {
        let capture = capture_with_height(measured).await;
        assert_eq!((capture.content_height, capture.captured_height), (content, captured), "measured {measured}");
    }
}

#[tokio::test]
async fn at_most_twenty_console_messages_are_kept_and_the_rest_are_counted() {
    let mut emitted = 0;
    let console = move || {
        emitted += 1;
        (0..25).map(|n| json!({"type": "log", "args": [{"type": "string", "value": format!("line {n} at {PAGE_URL}")}]})).chain((emitted > 0).then(|| json!({"type": "error", "args": [{"type": "string", "value": "y".repeat(600)}]}))).collect()
    };
    let browser = FakeBrowser::with_handler(page_handler(Arc::default(), |_| measured(100.0), console));
    let capture = within(capture_html_screenshot(&FakeLauncher::new(browser), capture_input("<p>x</p>", 500))).await.unwrap();
    assert_eq!(capture.console_messages.len(), 21);
    assert_eq!(capture.console_messages[0].text, "line 0 at page.html");
    assert_eq!(capture.console_messages[19].text, "line 19 at page.html");
    assert_eq!((capture.console_messages[20].level.as_str(), capture.console_messages[20].text.as_str()), ("warning", "6 more console messages were omitted."));
}

#[tokio::test]
async fn a_long_console_line_is_cut_with_an_ellipsis() {
    let console = || vec![json!({"type": "error", "args": [{"type": "string", "value": "y".repeat(600)}]})];
    let browser = FakeBrowser::with_handler(page_handler(Arc::default(), |_| measured(100.0), console));
    let capture = within(capture_html_screenshot(&FakeLauncher::new(browser), capture_input("<p>x</p>", 500))).await.unwrap();
    assert_eq!(capture.console_messages[0].text, format!("{}…", "y".repeat(500)));
}

#[tokio::test]
async fn each_width_is_measured_from_a_fresh_load_a_few_at_a_time() {
    let pages = Arc::new(Mutex::new(Pages::default()));
    // Like a D3 chart, the page sizes itself from the width once, at load.
    let browser = FakeBrowser::with_handler(page_handler(pages.clone(), |width| measured(f64::from(width) / 2.0), Vec::new));
    let launcher = FakeLauncher::new(browser);
    let widths = [320, 375, 430, 520, 640, 728, 860, 1000, 1144];
    let heights = within(measure_html_heights(&launcher, Path::new("/fake/chrome"), true, "<div id=chart></div>", &widths, FRAGMENT)).await.unwrap();
    assert_eq!(heights, widths.iter().map(|width| (*width, width.div_ceil(2))).collect::<Vec<_>>());
    let pages = pages.lock().unwrap();
    assert_eq!((pages.navigations, pages.created, pages.closed, pages.open), (9, 9, 9, 0), "one fresh page and one load for each width, each page freed");
    assert!((2..=3).contains(&pages.max_open), "{} pages open at once", pages.max_open);
    assert_eq!(launcher.launches().len(), 1, "one browser for all of them");
    assert!(launcher.launches()[0].1, "the sandbox setting is passed on");
    assert_eq!(launcher.shutdowns(), 1);
}

#[tokio::test]
async fn a_failing_page_fails_the_measurement_with_its_error() {
    let browser = FakeBrowser::with_handler(|command| match command.method.as_str() {
        "Target.createTarget" => vec![json!({"id": command.id, "error": {"code": -32000, "message": "Not allowed"}})],
        _ => vec![json!({"id": command.id, "result": {}})],
    });
    let launcher = FakeLauncher::new(browser);
    let error = within(measure_html_heights(&launcher, Path::new("/fake/chrome"), false, "<p>x</p>", &[320, 375], FRAGMENT)).await.unwrap_err();
    assert_eq!((error.reason.as_str(), error.cause.as_deref()), ("Target.createTarget failed", Some("Not allowed")));
    assert_eq!(error.message(), "Headless Chrome could not render the page: Target.createTarget failed.");
    assert_eq!(launcher.shutdowns(), 1, "the browser is ended after a failure too");
}

/// A browser that answers the setup of a page and then whatever `reply` says to `method`.
fn failing_at(method: &'static str, reply: Value) -> FakeBrowser {
    FakeBrowser::with_handler(move |command| {
        if command.method == method {
            let mut frame = reply.clone();
            frame["id"] = json!(command.id);
            return vec![frame];
        }
        match command.method.as_str() {
            "Target.createTarget" => vec![json!({"id": command.id, "result": {"targetId": "T-1"}})],
            "Target.attachToTarget" => vec![json!({"id": command.id, "result": {"sessionId": "S-1"}})],
            _ => vec![json!({"id": command.id, "sessionId": command.session_id, "result": {}})],
        }
    })
}

#[tokio::test]
async fn what_the_browser_refuses_or_gets_wrong_is_reported_with_the_command_it_concerns() {
    let cases = [
        ("Page.enable", json!({"error": {"code": -32601, "message": "'Page.enable' wasn't found"}}), "Page.enable failed", Some("'Page.enable' wasn't found")),
        ("Target.createTarget", json!({"result": {"noTarget": true}}), "Target.createTarget returned an unexpected result", None),
        ("Target.attachToTarget", json!({"result": {"sessionId": 5}}), "Target.attachToTarget returned an unexpected result", None),
        ("Page.navigate", json!({"result": {"frameId": "T-1", "errorText": "net::ERR_BLOCKED_BY_CLIENT"}}), "the page could not be opened", None),
        ("Page.navigate", json!({"result": "not an object"}), "Page.navigate returned an unexpected result", None),
    ];
    for (method, reply, reason, cause) in cases {
        let launcher = FakeLauncher::new(failing_at(method, reply));
        let error = within(capture_html_screenshot(&launcher, capture_input("<p>x</p>", 500))).await.unwrap_err();
        assert_eq!(error.reason, reason, "{method}");
        match cause {
            Some(cause) => assert_eq!(error.cause.as_deref(), Some(cause), "{method}"),
            None => assert!(error.reason.contains("unexpected result") || error.cause.is_none(), "{method}"),
        }
        assert_eq!(error.output, None, "{method}");
        assert_eq!(launcher.shutdowns(), 1, "{method}");
    }
}

#[tokio::test]
async fn a_measure_that_throws_is_an_unexpected_result() {
    // An exception comes back as a result without a value.
    let browser = FakeBrowser::with_handler(page_handler(Arc::default(), |_| json!({"result": {"type": "object", "subtype": "error"}, "exceptionDetails": {"text": "Uncaught"}}), Vec::new));
    let error = within(capture_html_screenshot(&FakeLauncher::new(browser), capture_input("<p>x</p>", 500))).await.unwrap_err();
    assert_eq!(error.reason, "Runtime.evaluate returned an unexpected result");
}

#[tokio::test]
async fn a_browser_that_goes_away_mid_load_reports_why_it_exited() {
    // It hangs up when asked to navigate.
    let browser = FakeBrowser::with_handler(|command| match command.method.as_str() {
        "Target.createTarget" => vec![json!({"id": command.id, "result": {"targetId": "T-1"}})],
        "Target.attachToTarget" => vec![json!({"id": command.id, "result": {"sessionId": "S-1"}})],
        "Page.navigate" => vec![json!({"$hangUp": true})],
        _ => vec![json!({"id": command.id, "sessionId": command.session_id, "result": {}})],
    });
    let launcher = FakeLauncher::new(browser).exiting_with("[ERROR] No usable sandbox!");
    let error = within(capture_html_screenshot(&launcher, capture_input("<p>x</p>", 500))).await.unwrap_err();
    assert_eq!((error.reason.as_str(), error.output.as_deref()), ("the browser exited unexpectedly", Some("[ERROR] No usable sandbox!")));
}

#[tokio::test]
async fn a_browser_that_goes_away_after_the_load_started_ends_the_wait_for_it() {
    // It acknowledges the navigation and then hangs up without ever loading the page.
    let browser = FakeBrowser::with_handler(|command| match command.method.as_str() {
        "Target.createTarget" => vec![json!({"id": command.id, "result": {"targetId": "T-1"}})],
        "Target.attachToTarget" => vec![json!({"id": command.id, "result": {"sessionId": "S-1"}})],
        "Page.navigate" => vec![json!({"id": command.id, "sessionId": "S-1", "result": {"frameId": "T-1"}}), json!({"$hangUp": true})],
        _ => vec![json!({"id": command.id, "sessionId": command.session_id, "result": {}})],
    });
    let error = within(capture_html_screenshot(&FakeLauncher::new(browser), capture_input("<p>x</p>", 500))).await.unwrap_err();
    assert_eq!(error.reason, "the browser exited unexpectedly");
}

#[tokio::test(start_paused = true)]
async fn a_page_that_never_finishes_loading_times_out_after_twenty_seconds_and_leaves_nothing_behind() {
    // The page is navigated and never loads.
    let browser = FakeBrowser::with_handler(|command| match command.method.as_str() {
        "Target.createTarget" => vec![json!({"id": command.id, "result": {"targetId": "T-1"}})],
        "Target.attachToTarget" => vec![json!({"id": command.id, "result": {"sessionId": "S-1"}})],
        "Page.navigate" => vec![json!({"id": command.id, "sessionId": "S-1", "result": {"frameId": "T-1"}})],
        _ => vec![json!({"id": command.id, "sessionId": command.session_id, "result": {}})],
    });
    let launcher = FakeLauncher::new(browser);
    let started = tokio::time::Instant::now();
    let error = capture_html_screenshot(&launcher, capture_input("<p>x</p>", 500)).await.unwrap_err();
    assert_eq!(error.reason, "the page did not finish loading within 20 seconds");
    assert_eq!(error.message(), "Headless Chrome could not render the page: the page did not finish loading within 20 seconds.");
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(20));
    assert!(!launcher.launches()[0].2.parent().unwrap().exists(), "the scratch directory is removed when the capture is abandoned");
}

#[tokio::test]
async fn every_load_serves_the_pages_utf8_bytes_as_base64() {
    let pages = Arc::new(Mutex::new(Pages::default()));
    let browser = FakeBrowser::with_handler(page_handler(pages, |_| measured(10.0), Vec::new));
    let launcher = FakeLauncher::new(browser);
    let html = "<p>\u{1F600} \u{e9}</p>";
    within(measure_html_heights(&launcher, Path::new("/fake/chrome"), false, html, &[320, 640], FRAGMENT)).await.unwrap();
    let expected = base64::engine::general_purpose::STANDARD.encode(html.as_bytes());
    let bodies: Vec<String> = launcher.browser.commands_named("Fetch.fulfillRequest").iter().map(|command| command.params["body"].as_str().unwrap().to_owned()).collect();
    assert_eq!(bodies, [expected.clone(), expected]);
}

#[cfg(unix)]
mod process {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// An executable script standing in for the browser. A script written an instant before it runs can meet "text file busy" when another
    /// test forks in between, so a start that fails that way is retried.
    fn script(directory: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = directory.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes()).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o755)).unwrap();
        file.sync_all().unwrap();
        drop(file);
        path
    }

    /// Said by a script once it has set itself up: it reads the first byte of the first command and answers it, so a client that has had its
    /// answer knows the script is ready for what the test does next.
    const READY: &str = "head -c 1 <&3 >/dev/null\nprintf '{\"id\":1,\"result\":{}}\\0' >&4";

    async fn launch(executable: &Path, no_sandbox: bool, profile: &Path) -> Result<LaunchedBrowser, HtmlRenderBrowserError> {
        let input = LaunchInput { executable: executable.to_owned(), no_sandbox, profile_directory: profile.to_owned() };
        let mut attempts = 0;
        loop {
            match ProcessLauncher.launch(&input).await {
                Err(error) if error.cause.as_deref().is_some_and(|cause| cause.contains("Text file busy")) && attempts < 20 => attempts += 1,
                other => return other,
            }
        }
    }

    #[tokio::test]
    async fn the_browser_is_started_with_the_pipe_on_fds_3_and_4_the_proxy_and_the_profile() {
        let directory = tempfile::tempdir().unwrap();
        // Says how it was started on stderr, then echoes what arrives on fd 3 to fd 4.
        let echo = script(directory.path(), "echo-browser", "printf '%s\\n' \"$@\" >&2\nexec cat <&3 >&4");
        let profile = directory.path().join("profile");
        let browser = launch(&echo, false, &profile).await.unwrap();
        // A command goes out on fd 3 and the browser's answer comes back on fd 4: here, the command itself, which the client reads as a reply.
        let reply = within(browser.connection.send("Echo.hello", json!({"a": [1, 2]}), None)).await.unwrap();
        assert_eq!(reply, Value::Null);
        browser.handle.shutdown().await;
        within(browser.connection.closed()).await;
        let exited = browser.handle.exited().await;
        let started: Vec<&str> = exited.output.as_deref().unwrap().lines().collect();
        let proxy = started.iter().find_map(|argument| argument.strip_prefix("--proxy-server=socks5://127.0.0.1:")).expect("a proxy flag");
        assert!(proxy.parse::<u16>().unwrap() > 0);
        let profile_flag = format!("--user-data-dir={}", profile.display());
        assert_eq!(
            started.iter().filter(|argument| !argument.starts_with("--proxy-server=")).copied().collect::<Vec<_>>(),
            [
                "--headless=new",
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-gpu",
                "--hide-scrollbars",
                "--mute-audio",
                "--block-new-web-contents",
                "--proxy-bypass-list=<-loopback>",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
                profile_flag.as_str(),
                "about:blank",
            ]
        );
    }

    /// TODO(original: the server browser has no network policy): its launcher starts the same shell with the network as the machine has it,
    /// popups allowed and no proxy. The HTML renderer's launcher above keeps its proxy and its blocked popups.
    #[tokio::test]
    async fn the_server_browsers_launcher_starts_the_shell_without_the_proxy_the_renderer_goes_through() {
        let directory = tempfile::tempdir().unwrap();
        let args = script(directory.path(), "direct-args-browser", "printf '%s\\n' \"$@\" >&2\nexit 0");
        let profile = directory.path().join("profile");
        let input = LaunchInput { executable: args.clone(), no_sandbox: false, profile_directory: profile.clone() };
        let mut attempts = 0;
        let browser = loop {
            match DirectProcessLauncher.launch(&input).await {
                Err(error) if error.cause.as_deref().is_some_and(|cause| cause.contains("Text file busy")) && attempts < 20 => attempts += 1,
                other => break other.unwrap(),
            }
        };
        within(browser.connection.closed()).await;
        let output = browser.handle.exited().await.output.unwrap();
        let started: Vec<&str> = output.lines().collect();
        let profile_flag = format!("--user-data-dir={}", profile.display());
        assert_eq!(
            started,
            [
                "--headless=new",
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-gpu",
                "--hide-scrollbars",
                "--mute-audio",
                "--force-device-scale-factor=2",
                "--disable-popup-blocking",
                "--disable-extensions",
                "--password-store=basic",
                "--use-mock-keychain",
                "--force-color-profile=srgb",
                profile_flag.as_str(),
                "about:blank",
            ]
        );
        assert!(!output.contains("proxy") && !output.contains("--block-new-web-contents"));
    }

    #[tokio::test]
    async fn the_sandbox_is_switched_off_only_when_asked() {
        let directory = tempfile::tempdir().unwrap();
        let echo = script(directory.path(), "args-browser", "printf '%s\\n' \"$@\" >&2\nexit 0");
        for no_sandbox in [false, true] {
            let browser = launch(&echo, no_sandbox, &directory.path().join("profile")).await.unwrap();
            within(browser.connection.closed()).await;
            let output = browser.handle.exited().await.output.unwrap();
            assert_eq!(output.lines().any(|line| line == "--no-sandbox"), no_sandbox);
            if no_sandbox {
                let lines: Vec<&str> = output.lines().collect();
                assert_eq!(lines[lines.len() - 3], "--no-sandbox", "after the flags and before the profile: {lines:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_browser_that_aborts_says_why_on_stderr_and_the_connection_closes() {
        let directory = tempfile::tempdir().unwrap();
        let aborting = script(directory.path(), "aborting-browser", "echo '[1:1:ERROR:zygote_host_impl_linux.cc] No usable sandbox! Update your kernel' >&2\nexit 1");
        let browser = launch(&aborting, false, &directory.path().join("profile")).await.unwrap();
        assert_eq!(within(browser.connection.send("Browser.getVersion", json!({}), None)).await, Err(crate::cdp::CdpError::Closed));
        let exited = browser.handle.exited().await;
        assert_eq!(exited.reason, "the browser exited unexpectedly");
        assert!(exited.output.unwrap().contains("No usable sandbox"));
    }

    #[tokio::test]
    async fn only_the_last_of_a_long_stderr_is_kept() {
        let directory = tempfile::tempdir().unwrap();
        let noisy = script(directory.path(), "noisy-browser", "i=0\nwhile [ $i -lt 400 ]; do echo \"line $i of the browser's complaints\" >&2; i=$((i+1)); done\nexit 0");
        let browser = launch(&noisy, false, &directory.path().join("profile")).await.unwrap();
        within(browser.connection.closed()).await;
        let output = browser.handle.exited().await.output.unwrap();
        assert!(output.encode_utf16().count() <= 2048 && output.ends_with("line 399 of the browser's complaints\n"), "{} units, ends {:?}", output.len(), &output[output.len().saturating_sub(40)..]);
    }

    #[tokio::test]
    async fn ending_the_browser_ends_everything_it_started() {
        let directory = tempfile::tempdir().unwrap();
        // A helper that holds the pipe open too, so the pipe only closes when the whole process group is gone.
        let parent = script(directory.path(), "group-browser", &format!("sleep 600 &\n{READY}\nwait"));
        let browser = launch(&parent, false, &directory.path().join("profile")).await.unwrap();
        within(browser.connection.send("Ready.ping", json!({}), None)).await.unwrap();
        assert!(!browser.connection.is_closed());
        browser.handle.shutdown().await;
        within(browser.connection.closed()).await;
    }

    #[tokio::test]
    async fn dropping_the_browser_kills_its_process_group_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let parent = script(directory.path(), "dropped-browser", &format!("sleep 600 &\n{READY}\nwait"));
        let browser = launch(&parent, false, &directory.path().join("profile")).await.unwrap();
        within(browser.connection.send("Ready.ping", json!({}), None)).await.unwrap();
        let connection = browser.connection.clone();
        drop(browser);
        within(connection.closed()).await;
    }

    #[tokio::test]
    async fn a_browser_that_ignores_the_request_to_end_is_killed_after_two_seconds() {
        let directory = tempfile::tempdir().unwrap();
        let stubborn = script(directory.path(), "stubborn-browser", &format!("trap '' TERM\n{READY}\nwhile :; do sleep 1; done"));
        let browser = launch(&stubborn, false, &directory.path().join("profile")).await.unwrap();
        // It has been told to ignore the request to end once it answers.
        within(browser.connection.send("Ready.ping", json!({}), None)).await.unwrap();
        let started = tokio::time::Instant::now();
        browser.handle.shutdown().await;
        assert!(started.elapsed() >= std::time::Duration::from_secs(2));
        within(browser.connection.closed()).await;
    }

    #[tokio::test]
    async fn a_browser_that_cannot_be_started_is_an_error_and_leaves_no_proxy_behind() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("no-such-browser");
        let error = launch(&missing, false, &directory.path().join("profile")).await.err().expect("a failed start");
        assert_eq!(error.reason, "the browser could not be started");
        assert!(error.cause.is_some());
        let not_executable = directory.path().join("not-executable");
        std::fs::write(&not_executable, "#!/bin/sh\n").unwrap();
        let error = launch(&not_executable, false, &directory.path().join("profile")).await.err().expect("a failed start");
        assert_eq!(error.reason, "the browser could not be started");
    }

    #[tokio::test]
    async fn the_proxy_the_browser_is_given_refuses_this_machine_and_ends_with_the_browser() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let echoing = script(directory.path(), "proxy-browser", "printf '%s\\n' \"$@\" >&2\nexec cat <&3 >&4");
        let browser = launch(&echoing, false, &directory.path().join("profile")).await.unwrap();
        // The script says how it was started before it starts echoing, so one round trip means it has. Its stderr stays open, so the diagnosis
        // waits out its second for the end of it, and then gives what was written.
        within(browser.connection.send("Echo.ready", json!({}), None)).await.unwrap();
        let output = browser.handle.exited().await.output.unwrap();
        let port: u16 = output.lines().find_map(|line| line.strip_prefix("--proxy-server=socks5://127.0.0.1:")).expect("a proxy flag").parse().unwrap();
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client.write_all(&[5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1, 0, 80]).await.unwrap();
        let mut answer = [0u8; 12];
        within(client.read_exact(&mut answer)).await.unwrap();
        assert_eq!((answer[0..2].to_vec(), answer[3]), (vec![5, 0], 2), "loopback is refused");
        // The proxy ends with the browser that owns it, and takes the connections it carries along.
        drop(browser);
        let mut rest = Vec::new();
        let _ = within(client.read_to_end(&mut rest)).await;
        assert!(rest.is_empty());
    }
}
