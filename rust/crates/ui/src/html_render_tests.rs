//! An agent's HTML render in the conversation, over the real server: the page of an uploaded attachment is framed in a
//! sandbox with an opaque origin, wears the app theme from its address, opens in the right panel from its row, and a page that
//! cannot be loaded holds its box and says so. The `html_render` tool itself is the server's (`mcp_html`); the fake providers do
//! not call it, so the tool's result is put into the thread's projection the way the server would report it.
use super::*;
use crate::runtime;
use crate::diff_panel::Gate;
use crate::runtime::diff_tests::{Fixture, Harness, open_thread, rendered_text, seed, until};
use crate::runtime::transport_tests::{click_control, control, control_attribute};
use std::{cell::RefCell, rc::Rc};

const PAGE: &str = "<!doctype html><title>Revenue</title><p>Quarterly revenue</p>";

/// Uploads `PAGE` as an attachment of the server, the way the composer does, and returns its id.
async fn upload_page(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>) -> String {
    let uploaded = Rc::new(RefCell::new(None::<Result<String, String>>));
    let done = Rc::new(tokio::sync::Notify::new());
    {
        let (transport, uploaded, done) = (props.transport.clone(), uploaded.clone(), done.clone());
        dom.in_scope(ScopeId::APP, || {
            dioxus::dioxus_core::spawn_forever(async move {
                let result = async {
                    let minted = runtime::request_value(transport.clone(), state, "attachments.createUploadUrl", json!({"name":"Revenue.html","mimeType":"text/html","sizeBytes":PAGE.len(),"type":"file"})).await?;
                    let environment = state.destination().peek().as_ref().map(ToString::to_string).ok_or("no environment")?;
                    let url = runtime::http_url(&transport, state, &environment, minted["relativeUrl"].as_str().ok_or("no url")?).ok_or("not connected")?;
                    let response = reqwest::Client::new().post(url).header("content-type", "text/html").body(PAGE).send().await.map_err(|error| error.to_string())?;
                    if !response.status().is_success() {
                        return Err(format!("upload refused: {}", response.status()));
                    }
                    Ok(minted["attachmentId"].as_str().ok_or("no id")?.to_owned())
                }
                .await;
                *uploaded.borrow_mut() = Some(result);
                done.notify_one();
            });
        });
    }
    // The task touches no signal, so the document has no work to wake it: the task says when it is done.
    while uploaded.borrow().is_none() {
        tokio::select! {
            _ = dom.wait_for_work() => dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations),
            _ = done.notified() => {}
        }
    }
    let id = uploaded.borrow_mut().take().unwrap().expect("upload");
    id
}

/// Puts a completed `html_render` call into the open thread, as the server reports one.
fn publish_page(state: Store<UiModel>, id: &str, attachment_id: &str, title: &str, extra: Value) {
    let mut output = json!({"htmlRender":{"attachmentId":attachment_id,"title":title,"height":300}});
    for (key, value) in extra.as_object().unwrap() {
        output[key] = value.clone();
    }
    let item = json!({"id":id,"type":"dynamic_tool","status":"completed","threadId":"thread-a","runId":"run-1","ordinal":0,"toolName":"mcp__t3-code__html_render","output":output,"updatedAt":"2026-01-01T00:00:00Z","startedAt":"2026-01-01T00:00:00Z"});
    let row = json!({"sourceThreadId":"thread-a","sourceItemId":id,"position":0,"visibility":"local","item":item});
    let mut domain = state.thread();
    let mut thread = domain.write();
    let projection = thread.projection.as_mut().expect("projection");
    projection["visibleTurnItems"].as_array_mut().unwrap().push(row);
    projection["turnItems"].as_array_mut().unwrap().push(item);
}

#[tokio::test(flavor = "current_thread")]
async fn a_published_page_is_framed_in_a_sandbox_on_the_app_theme_and_opens_in_the_panel() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let attachment = upload_page(&mut dom, &props, state).await;
    publish_page(state, "page-1", &attachment, "Revenue", json!({}));
    until(&mut dom, "the page's frame", |dom| control_attribute(dom, "Revenue", "src").is_some()).await;

    // Scripts and forms, and never the app's origin: the page cannot reach its session or storage.
    let sandbox = control_attribute(&dom, "Revenue", "sandbox").and_then(|(_, value)| value).expect("sandbox");
    assert_eq!(sandbox, "allow-scripts allow-forms");
    let src = control_attribute(&dom, "Revenue", "src").and_then(|(_, value)| value).expect("src");
    assert!(src.starts_with("http://") && src.contains("/api/assets/"), "{src}");
    assert!(src.contains("#t3-theme=%7B%22appearance%22%3A%22"), "the theme travels in the fragment: {src}");
    assert!(control(&dom, "Open in panel").is_some());

    click_control(&mut dom, "Open in panel");
    until(&mut dom, "the page's tab", |dom| control(dom, "Revenue.html").is_some() && control(dom, "Download Revenue.html").is_some()).await;
    let download = control_attribute(&dom, "Download Revenue.html", "href").and_then(|(_, value)| value).expect("download link");
    assert!(download.contains("/api/assets/"), "{download}");
    // Opening it again shows the tab it has.
    click_control(&mut dom, "Open in panel");
    assert_eq!(crate::runtime::transport_tests::all_controls(&dom, "Revenue.html").len(), 1);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_page_that_cannot_be_loaded_says_so_in_the_box_it_holds() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    publish_page(state, "page-2", "no-such-attachment", "Gone", json!({}));
    until(&mut dom, "the failure", |dom| rendered_text(dom).contains("Unable to load Gone")).await;
    assert!(control(&dom, "Gone").is_none(), "no frame was made");
    assert!(control(&dom, "Open in panel").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_html_render_call_is_the_tool_call_it_is_and_has_no_frame() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    publish_page(state, "page-3", "any", "Broken", json!({"isError":true}));
    // The call is work like any other: its settled turn folds it behind a line, and no frame is made for it.
    until(&mut dom, "the settled turn's line", |dom| rendered_text(dom).contains("Worked for")).await;
    assert!(control(&dom, "Broken").is_none() && !rendered_text(&dom).contains("Unable to load"), "{}", rendered_text(&dom));
    assert!(control(&dom, "Open in panel").is_none());
    fixture.finish(dom).await;
}
