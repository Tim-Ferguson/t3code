//! The HTML tools end to end through `McpControlTools::call`, against a scripted browser: what the agent gets back, in what shape, and what it
//! is told when something goes wrong. No browser is started or downloaded.
use crate::{
    cdp_fake::{FakeBrowser, FakeLauncher},
    html_render::{BrowserSource, HtmlRender, HtmlRenderOptions, provide_for_store},
    mcp_control::McpControlTools,
    mcp_invocation::InvocationScope,
    persistence::Store,
    preview_browser_install::PreviewBrowserError,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

struct Browser(Result<PathBuf, PreviewBrowserError>);
#[async_trait]
impl BrowserSource for Browser {
    fn installed(&self) -> Option<PathBuf> {
        self.0.as_ref().ok().cloned()
    }
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError> {
        self.0.clone()
    }
}

struct Env {
    tools: McpControlTools,
    scope: InvocationScope,
    attachments: PathBuf,
    launcher: Arc<FakeLauncher>,
    _directory: tempfile::TempDir,
}

fn env(browser: Result<PathBuf, PreviewBrowserError>) -> Env {
    let directory = tempfile::tempdir().unwrap();
    let (store, scope) = crate::mcp_invocation::tests::fixture_with(Store::open(directory.path().join("rust-state.sqlite")).unwrap());
    // The caller owns a live run, so the tools that act as it are let through.
    let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
    projection["runs"] = json!([{"id":"run:1","status":"running"}]);
    store.transaction(|tx| crate::persistence::write_projection(tx, "thread", "thread:1", &projection)).unwrap();
    let attachments = directory.path().join("attachments");
    let launcher = Arc::new(FakeLauncher::new(FakeBrowser::replay(include_str!("../tests/fixtures/cdp-html-capture.jsonl"))));
    provide_for_store(&store, HtmlRender::new(HtmlRenderOptions { browser: Arc::new(Browser(browser)), launcher: launcher.clone(), attachments_dir: attachments.clone(), no_sandbox: false, setup_command: "sudo t3 browser setup".into() }));
    let mut scope = scope;
    // The HTML tools are the caller's own; they need no browser capability.
    scope.capabilities.clear();
    Env { tools: McpControlTools { store, execution: None, clock: Arc::new(chrono::Utc::now), services: Default::default() }, scope, attachments, launcher, _directory: directory }
}

fn installed() -> Result<PathBuf, PreviewBrowserError> {
    Ok("/fake/chrome-headless-shell".into())
}

#[tokio::test]
async fn a_preview_answers_with_the_metadata_as_text_and_the_screenshot_as_an_image() {
    let env = env(installed());
    let result = env.tools.call(&env.scope, "html_preview", json!({"html":"<p>x</p>","width":400,"appearance":"light"})).await.unwrap();
    assert_eq!(result["isError"], false);
    let metadata = json!({
        "width": 400, "contentHeight": 301, "capturedHeight": 301,
        "consoleMessages": [
            {"level": "log", "text": "ready 3"},
            {"level": "info", "text": "\"Liberation Sans\", Arimo, Arial, Helvetica, sans-serif"},
            {"level": "warning", "text": "#0a0a0a true null undefined Object"},
            {"level": "error", "text": "Error: broken chart\n    at page.html:1:7"},
            {"level": "error", "text": "Failed to load resource: net::ERR_FAILED http://t3-page.localhost/missing.png"},
        ],
        "screenshot": {"mimeType": "image/png", "width": 400, "height": 301},
    });
    assert_eq!(result["structuredContent"], metadata);
    // The keys are in the order the original's success schema writes them, and the text is the metadata as JSON.
    assert_eq!(result["structuredContent"].as_object().unwrap().keys().collect::<Vec<_>>(), ["width", "contentHeight", "capturedHeight", "consoleMessages", "screenshot"]);
    assert_eq!(result["content"][0], json!({"type":"text","text":metadata.to_string()}));
    assert_eq!(result["content"][1], json!({"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}));
    assert_eq!(result["content"].as_array().unwrap().len(), 2);
    assert_eq!(env.launcher.browser.commands_named("Emulation.setDeviceMetricsOverride")[0].params["width"], 400);
}

#[tokio::test]
async fn a_preview_names_the_images_it_could_not_read() {
    let env = env(installed());
    let result = env.tools.call(&env.scope, "html_preview", json!({"html":"<img src=\"/nonexistent/t3-missing.png\">"})).await.unwrap();
    assert_eq!(result["structuredContent"]["missingImages"], json!(["/nonexistent/t3-missing.png"]));
    let keys: Vec<&String> = result["structuredContent"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["width", "contentHeight", "capturedHeight", "consoleMessages", "missingImages", "screenshot"]);
}

#[tokio::test]
async fn a_preview_that_cannot_run_fails_in_the_shape_of_an_image_tool() {
    let installing = PreviewBrowserError::Installing { downloaded_bytes: 50_000_000, total_bytes: 99_221_129, unpacking: false };
    let env = env(Err(installing));
    let result = env.tools.call(&env.scope, "html_preview", json!({"html":"<p>x</p>"})).await.unwrap();
    let message = "T3 is installing its headless browser (50 of 99 MB downloaded). Try again in a minute.";
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"], json!({"error":{"_tag":"OrchestratorMcpFailure","operation":"preview","failureCount":1,"message":message}}));
    assert_eq!(result["content"], json!([{"type":"text","text":message}]));
}

#[tokio::test]
async fn a_caller_outside_a_thread_is_refused_before_anything_is_rendered() {
    let mut env = env(installed());
    env.scope.thread = None;
    for (tool, arguments) in [("html_preview", json!({"html":"<p>x</p>"})), ("html_render", json!({"html":"<p>x</p>","title":"x","height":200}))] {
        let result = env.tools.call(&env.scope, tool, arguments).await.unwrap();
        assert_eq!(result["isError"], true, "{tool}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("thread_credential_required") || text.contains("acts as the calling T3 thread, so it needs an agent running inside T3 Code"), "{tool}: {text}");
    }
    assert!(env.launcher.launches().is_empty());
    assert!(!env.attachments.exists());
}

#[tokio::test]
async fn an_installation_without_a_home_cannot_render_and_says_so_plainly() {
    let (store, mut scope) = crate::mcp_invocation::tests::fixture();
    scope.capabilities.clear();
    let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
    projection["runs"] = json!([{"id":"run:1","status":"running"}]);
    store.transaction(|tx| crate::persistence::write_projection(tx, "thread", "thread:1", &projection)).unwrap();
    let tools = McpControlTools { store, execution: None, clock: Arc::new(chrono::Utc::now), services: Default::default() };
    let result = tools.call(&scope, "html_preview", json!({"html":"<p>x</p>"})).await.unwrap();
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "The operation could not be completed.");
    let result = tools.call(&scope, "html_render", json!({"html":"<p>x</p>","title":"x","height":200})).await.unwrap();
    assert_eq!(result["isError"], true);
    let failure: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(failure, json!({"_tag":"OrchestratorMcpFailure","code":"orchestration_error","message":"The operation could not be completed."}));
}

#[tokio::test]
async fn a_publish_stores_the_page_in_the_callers_thread_and_answers_with_its_reference() {
    let env = env(Err(PreviewBrowserError::Installing { downloaded_bytes: 1, total_bytes: 2, unpacking: true }));
    let result = env.tools.call(&env.scope, "html_render", json!({"html":"<p>Quarterly revenue</p>","title":"  Revenue ","height":9000})).await.unwrap();
    assert_eq!(result["isError"], false);
    let structured = &result["structuredContent"];
    let reference = &structured["htmlRender"];
    let id = reference["attachmentId"].as_str().unwrap();
    assert!(id.starts_with("thread-1-") && id.ends_with("-html"), "{id}");
    assert_eq!(*reference, json!({"attachmentId": id, "title": "Revenue", "height": 2000}));
    assert_eq!(structured["message"], "Shown to the reader above your reply. Don't mention or describe the page; reply with only what it doesn't already say.");
    assert_eq!(result["content"][0]["text"], structured.to_string());
    let stored = std::fs::read_to_string(env.attachments.join(format!("{id}.html"))).unwrap();
    assert!(stored.contains("<p>Quarterly revenue</p>"));
    // The browser was never asked for: with none installed, a publish goes without heights.
    assert!(env.launcher.launches().is_empty());
}

#[tokio::test]
async fn a_publish_the_agent_has_to_fix_says_so_as_the_originals_failure_value() {
    let env = env(installed());
    let result = env.tools.call(&env.scope, "html_render", json!({"html":"<img src=\"/nonexistent/t3-missing.png\">","title":"x","height":300})).await.unwrap();
    assert_eq!(result["isError"], true);
    let failure: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        failure,
        json!({"_tag":"OrchestratorMcpFailure","code":"invalid_request","message":"These local images could not be read: /nonexistent/t3-missing.png. Use absolute paths to existing image files, or remove them."})
    );
    assert!(!env.attachments.exists() || std::fs::read_dir(&env.attachments).unwrap().next().is_none(), "nothing was stored");
}

#[tokio::test]
async fn input_the_original_refuses_is_refused_with_its_text_before_any_work() {
    let env = env(installed());
    for (tool, arguments) in [
        ("html_preview", json!({})),
        ("html_preview", json!({"html":"<p>x</p>","width":"wide"})),
        ("html_preview", json!({"html":"<p>x</p>","appearance":"sepia"})),
        ("html_render", json!({"html":"<p>x</p>","title":"","height":200})),
        ("html_render", json!({"html":"<p>x</p>","title":"x","height":1.5})),
        ("html_render", json!({"html":"","title":"x","height":200})),
    ] {
        let error = env.tools.call(&env.scope, tool, arguments.clone()).await.unwrap_err();
        assert!(error.starts_with(&format!("Invalid parameters for tool '{tool}': ")), "{tool} {arguments}: {error}");
    }
    assert!(env.launcher.launches().is_empty());
}
