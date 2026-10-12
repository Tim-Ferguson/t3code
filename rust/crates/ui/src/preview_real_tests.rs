//! The Browser tab against the real `ServerBrowser` of the server crate, on its scripted Chrome (`cdp_fake`, `server_browser_double`; no
//! browser is started). `preview_tests` plays the server's side of the stream with a stand-in; here the tab, the control, the page's
//! dialog, picker and downloads are the server's own, and the test sees them only in the panel and in what the scripted Chrome was sent.
use super::*;
use async_trait::async_trait;
use std::sync::Arc;
use t3_server::{
    preview_automation_broker::PreviewAutomationBroker,
    preview_browser_install::PreviewBrowserError,
    server_browser::{ServerBrowser, ServerBrowserOptions},
    server_browser_contexts::{BrowserExecutable, ContextsOptions, ServerBrowserContexts},
    server_browser_double::{Double, DoubleLauncher},
};

struct Fixed;
#[async_trait]
impl BrowserExecutable for Fixed {
    async fn executable(&self) -> Result<PathBuf, PreviewBrowserError> {
        Ok(PathBuf::from("/fake/chrome-headless-shell"))
    }
}

/// The server's browser over the scripted Chrome, and what a test needs to reach either.
struct Real {
    browser: ServerBrowser,
    double: Double,
    launcher: Arc<DoubleLauncher>,
    downloads: PathBuf,
    /// Where the server keeps each profile's storage.
    profiles: PathBuf,
}
impl Real {
    /// The scripted Chrome the first launch started.
    fn chrome(&self) -> t3_server::cdp_fake::FakeBrowser {
        self.launcher.browser(0)
    }
    /// The session the tab's page was set up on (the viewer's screencast has a session of its own).
    fn session(&self) -> String {
        self.chrome().commands_named("Page.enable")[0].session_id.clone().expect("the page is set up on a session")
    }
    /// The commands of this method the scripted Chrome has had; none while it has not been started.
    fn commands_named(&self, method: &str) -> Vec<t3_server::cdp_fake::Command> {
        self.launcher.browsers.lock().unwrap().first().map(|chrome| chrome.commands_named(method)).unwrap_or_default()
    }
    fn page_event(&self, method: &str, params: Value) {
        self.chrome().push(json!({"method": method, "sessionId": self.session(), "params": params}));
    }
}

/// A server that hosts browser tabs, with the real server browser installed over the scripted Chrome.
async fn serving_real() -> (Fixture, Real) {
    serving_real_finding(None).await
}

/// As `serving_real`, on a machine where `discovery` finds the local servers.
async fn serving_real_finding(discovery: Option<t3_server::resource_discovery::PortDiscovery>) -> (Fixture, Real) {
    let fixture = match discovery {
        Some(discovery) => Fixture::start_discovering(json!({"serverBrowser": true}), discovery).await,
        None => Fixture::start_offering(json!({"serverBrowser": true})).await,
    };
    let root = fixture.directory.path().join("real-browser");
    let launcher = Arc::new(DoubleLauncher::new(Double::new()));
    let double = launcher.double.clone();
    let downloads = root.join("downloads");
    let contexts = ServerBrowserContexts::new(ContextsOptions {
        profiles_dir: root.join("profiles"),
        downloads_dir: downloads.clone(),
        executable: Arc::new(Fixed),
        launcher: launcher.clone(),
        no_sandbox: false,
        setup_command: "sudo t3 browser setup".into(),
        scratch_root: root.clone(),
    });
    let browser = ServerBrowser::start(ServerBrowserOptions {
        contexts,
        manager: fixture.store.extension(PreviewManager::default),
        broker: fixture.store.extension(PreviewAutomationBroker::default),
        environment_id: "diff-ui".into(),
        downloads_dir: downloads.clone(),
        attachments_dir: root.join("attachments"),
        setup_command: "sudo t3 browser setup".into(),
        sandbox_blocked: false,
    });
    t3_server::server_browser_http::install(&fixture.store, Arc::new(browser.clone()));
    TEST_PAGE.with(|page| *page.borrow_mut() = Some(Box::new(driver)));
    (fixture, Real { browser, double, launcher, downloads, profiles: root.join("profiles") })
}

async fn finish(real: Real, fixture: Fixture, dom: VirtualDom) {
    done();
    drop(dom);
    real.browser.shutdown().await;
    fixture.finish_server().await;
}

fn sessions_of(fixture: &Fixture) -> Vec<Value> {
    let list = fixture.store.extension(PreviewManager::default).list(serde_json::from_value(json!({"threadId": "thread-a"})).unwrap());
    serde_json::to_value(list).unwrap()["sessions"].as_array().unwrap().clone()
}

/// Waits for something only the server and the scripted Chrome change, while the window keeps rendering.
async fn server_side(dom: &mut VirtualDom, what: &str, ready: impl Fn() -> bool) {
    crate::runtime::keys_tests::poll_until(dom, what, |_| ready()).await;
}

/// One tab, taken over by the person: the page loaded at an address, with a title, and a first frame shown.
async fn browse(fixture: &Fixture, real: &Real) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    let (mut dom, props, state) = ready(fixture).await;
    real.double.on_evaluate("document.title", json!("Dev app"));
    // No agent owns a tab the person opens, so the viewer that attached first controls it.
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "the launcher", |dom| control(dom, "Open Browser").is_some()).await;
    click_when_enabled(&mut dom, "Open Browser").await;
    until(&mut dom, "the browser tab", |dom| control(dom, "Search or enter URL").is_some()).await;
    until(&mut dom, "the stream", |dom| control(dom, "Release control").is_some()).await;
    (dom, props, state)
}

#[tokio::test(flavor = "current_thread")]
async fn the_real_browser_streams_its_first_frame_and_hands_control_to_the_panel() {
    let (fixture, real) = serving_real().await;
    let (mut dom, _props, _state) = browse(&fixture, &real).await;
    // The tab is a session of the server's manager and a page of the scripted Chrome, sized to the panel.
    assert_eq!(sessions_of(&fixture).len(), 1);
    assert_eq!(real.chrome().commands_named("Page.startScreencast")[0].params["maxWidth"], 800.0);
    // The still the server sends an idle page ends the loading state; the first viewer of a tab nobody owns controls it.
    until(&mut dom, "the first frame", |dom| control(dom, "Refresh").is_some()).await;
    assert!(rendered_text(&dom).contains("You have control"));
    // A frame the browser sends is acked to it once the panel has shown it.
    let before = real.chrome().commands_named("Page.screencastFrameAck").len();
    let screencast = real.chrome().commands_named("Page.startScreencast")[0].session_id.clone();
    real.chrome().push(json!({"method": "Page.screencastFrame", "sessionId": screencast, "params": {"data": "/9j/2Q==", "sessionId": 7, "metadata": {"deviceWidth": 1280, "deviceHeight": 800}}}));
    server_side(&mut dom, "the ack", || real.chrome().commands_named("Page.screencastFrameAck").len() > before).await;
    assert_eq!(real.chrome().commands_named("Page.screencastFrameAck").last().unwrap().params["sessionId"], 7);

    click_when_enabled(&mut dom, "Release control").await;
    until(&mut dom, "watching again", |dom| rendered_text(dom).contains("Watching") && control(dom, "Take control").is_some()).await;
    finish(real, fixture, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn typing_an_address_and_keys_in_the_panel_reach_the_page_only_while_the_person_has_control() {
    let (fixture, real) = serving_real().await;
    let (mut dom, _props, _state) = browse(&fixture, &real).await;
    input_when_enabled(&mut dom, "Search or enter URL", "localhost:5173/app").await;
    key_on(&mut dom, "Search or enter URL", Key::Enter);
    // The server checked the address, sent the page there and told the panel where it landed and what the page is called.
    until(&mut dom, "the title", |dom| control(dom, "Close Dev app tab").is_some()).await;
    let urls: Vec<String> = real.chrome().commands_named("Page.navigate").iter().map(|command| command.params["url"].as_str().unwrap().to_owned()).collect();
    assert_eq!(urls.last().map(String::as_str), Some("http://localhost:5173/app"));
    assert_eq!(sessions_of(&fixture)[0]["navStatus"]["url"], "http://localhost:5173/app");
    assert_eq!(control(&dom, "Search or enter URL").and_then(|(_, value)| value).as_deref(), Some("http://localhost:5173/app"));
    // Reload goes to the page the way the viewer asked for it.
    assert_eq!(press(&mut dom, chord("r", false, false), Dom::default()).await.as_deref(), Some("preview.refresh"));
    server_side(&mut dom, "the reload", || !real.chrome().commands_named("Page.reload").is_empty()).await;
    // An address the server refuses is not navigated to.
    input_when_enabled(&mut dom, "Search or enter URL", "file:///etc/passwd").await;
    key_on(&mut dom, "Search or enter URL", Key::Enter);
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("Invalid preview URL")).await;
    assert!(real.chrome().commands_named("Page.navigate").iter().all(|command| !command.params["url"].as_str().unwrap().starts_with("file:")));
    finish(real, fixture, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_dialog_a_file_picker_and_a_download_of_the_real_page_are_answered_in_the_panel() {
    use dioxus_html::{SerializedFileData, SerializedFormData, SerializedFormObject};
    let (fixture, real) = serving_real().await;
    let (mut dom, _props, _state) = browse(&fixture, &real).await;

    // The page opens a prompt; the panel shows it with the page's default and the answer goes to the page.
    real.page_event("Page.javascriptDialogOpening", json!({"type": "prompt", "message": "Name?", "defaultPrompt": "Ada", "url": "http://x/"}));
    until(&mut dom, "the dialog", |dom| control(dom, "Browser dialog").is_some() && control(dom, "Dialog response").and_then(|(_, value)| value).as_deref() == Some("Ada")).await;
    assert!(rendered_text(&dom).contains("Name?"));
    crate::runtime::transport_tests::input_control(&mut dom, "Dialog response", "Grace");
    click_control(&mut dom, "Accept dialog");
    server_side(&mut dom, "the page's dialog", || !real.chrome().commands_named("Page.handleJavaScriptDialog").is_empty()).await;
    assert_eq!(real.chrome().commands_named("Page.handleJavaScriptDialog")[0].params, json!({"accept": true, "promptText": "Grace"}));
    until(&mut dom, "no dialog", |dom| control(dom, "Browser dialog").is_none()).await;

    // The page's picker: a file of this device goes to the page as bytes the server keeps in a folder made for it.
    real.double.on_evaluate("getAttribute", json!(".txt"));
    real.page_event("Page.fileChooserOpened", json!({"frameId": "T-1", "mode": "selectSingle", "backendNodeId": 9}));
    until(&mut dom, "the picker", |dom| control(dom, "Choose files for the page").is_some() && rendered_text(dom).contains("The page asks for a file.")).await;
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(&dom, "Choose a file").expect("the file input");
    let file = SerializedFileData { path: PathBuf::from("/picked/notes.txt"), size: 5, last_modified: 0, content_type: Some("text/plain".into()), contents: Some(b"hello".to_vec().into()) };
    let form = SerializedFormData::new(String::new(), vec![SerializedFormObject { key: "files".into(), text: None, file: Some(file) }]);
    dom.runtime().handle_event("change", dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(form))) as Rc<dyn std::any::Any>, true), element);
    crate::runtime::diff_tests::pump(&mut dom).await;
    server_side(&mut dom, "the upload", || !real.chrome().commands_named("DOM.setFileInputFiles").is_empty()).await;
    let given = real.chrome().commands_named("DOM.setFileInputFiles");
    assert_eq!(given[0].params["backendNodeId"], 9);
    let path = PathBuf::from(given[0].params["files"][0].as_str().unwrap());
    assert!(path.starts_with(real.downloads.join("uploads")) && path.ends_with("0/notes.txt"), "{path:?}");
    // The server removes the folder as soon as Chrome has been given the file, so its bytes cannot be read here without racing that.
    until(&mut dom, "the picker is answered", |dom| control(dom, "Choose files for the page").is_none()).await;

    // A file the page downloaded is offered; saving it reads it from the server.
    let incoming = real.downloads.join("incoming");
    std::fs::create_dir_all(&incoming).unwrap();
    std::fs::write(incoming.join("guid-1"), "a,b\n").unwrap();
    real.chrome().push(json!({"method": "Browser.downloadWillBegin", "params": {"frameId": "T-1", "guid": "guid-1", "url": "http://localhost/report.csv", "suggestedFilename": "report.csv"}}));
    real.chrome().push(json!({"method": "Browser.downloadProgress", "params": {"guid": "guid-1", "state": "completed", "totalBytes": 4, "receivedBytes": 4}}));
    until(&mut dom, "the download", |dom| rendered_text(dom).contains("Downloaded report.csv")).await;
    click_control(&mut dom, "Save report.csv");
    server_side(&mut dom, "the file to be kept", || !crate::media_save::SAVED.with(|saved| saved.borrow().is_empty())).await;
    assert_eq!(crate::media_save::SAVED.with(|saved| saved.borrow().clone()), vec![("report.csv".to_owned(), b"a,b\n".to_vec())]);
    finish(real, fixture, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn closing_the_tab_ends_the_page_of_the_real_browser() {
    let (fixture, real) = serving_real().await;
    let (mut dom, _props, state) = browse(&fixture, &real).await;
    input_when_enabled(&mut dom, "Search or enter URL", "localhost:5173").await;
    key_on(&mut dom, "Search or enter URL", Key::Enter);
    until(&mut dom, "the title", |dom| control(dom, "Close Dev app tab").is_some()).await;
    assert_eq!(real.browser.tab_count(), 1);

    click_control(&mut dom, "Close Dev app tab");
    server_side(&mut dom, "the page to be closed", || real.browser.tab_count() == 0 && !real.chrome().commands_named("Target.closeTarget").is_empty()).await;
    assert!(sessions_of(&fixture).is_empty());
    until(&mut dom, "the panel is gone", |dom| control(dom, "Search or enter URL").is_none()).await;
    // Reopening brings back a page at the address it was left at.
    assert_eq!(press(&mut dom, chord("t", true, false), Dom::default()).await.as_deref(), Some("view.reopenClosed"));
    server_side(&mut dom, "the new page", || real.browser.tab_count() == 1).await;
    until(&mut dom, "its address", |dom| control(dom, "Search or enter URL").and_then(|(_, value)| value).as_deref() == Some("http://localhost:5173/")).await;
    assert!(!crate::right_panel::can_reopen_peek(state));
    finish(real, fixture, dom).await;
}

/// Changes this device's settings as the settings page does.
fn set_client_settings(state: Store<UiModel>, change: Value) {
    let mut merged = serde_json::to_value(&*state.client_settings().peek()).unwrap();
    merged.as_object_mut().unwrap().extend(change.as_object().unwrap().clone());
    state.client_settings().set(serde_json::from_value(merged).unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_tab_starts_at_the_default_zoom_and_appearance_and_a_tab_with_the_defaults_is_left_alone() {
    let (fixture, real) = serving_real().await;
    let (mut dom, props, state) = ready(&fixture).await;
    set_client_settings(state, json!({"browserDefaultZoomFactor": 1.25, "browserDefaultAppearance": "dark"}));
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "the launcher", |dom| control(dom, "Open Browser").is_some()).await;
    click_when_enabled(&mut dom, "Open Browser").await;
    // The server holds the tab at the zoom and scheme the settings name.
    server_side(&mut dom, "the defaults", || sessions_of(&fixture).first().is_some_and(|session| session["zoomFactor"] == 1.25 && session["colorScheme"] == "dark")).await;
    finish(real, fixture, dom).await;

    let (fixture, real) = serving_real().await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "the launcher", |dom| control(dom, "Open Browser").is_some()).await;
    click_when_enabled(&mut dom, "Open Browser").await;
    until(&mut dom, "the tab", |dom| control(dom, "Search or enter URL").is_some()).await;
    assert!(real.chrome().commands_named("Emulation.setEmulatedMedia").is_empty(), "nothing is changed on a tab whose defaults are its own");
    assert!(sessions_of(&fixture)[0].get("zoomFactor").is_none_or(|zoom| *zoom == 1.0));
    finish(real, fixture, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_link_in_a_message_opens_in_a_tab_of_the_thread_only_when_the_setting_says_so() {
    let (fixture, real) = serving_real().await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let transport = props.transport.clone();
    let click = |dom: &mut VirtualDom, href: &str, held: bool| dom.in_scope(ScopeId::APP, || crate::link_menus::open_in_app(state, &transport, "thread-a", href, held));
    // The system's browser is the default: the click is the page's.
    assert!(!click(&mut dom, "http://localhost:5173/docs", false));
    set_client_settings(state, json!({"browserLinkTarget": "app"}));
    // Command or Control is the way out, and a link that is not a web page is the system's.
    assert!(!click(&mut dom, "http://localhost:5173/docs", true));
    assert!(!click(&mut dom, "mailto:someone@example.com", false));
    assert_eq!(sessions_of(&fixture).len(), 0);
    assert!(click(&mut dom, "http://localhost:5173/docs", false));
    server_side(&mut dom, "the page", || real.commands_named("Page.navigate").iter().any(|command| command.params["url"] == "http://localhost:5173/docs")).await;
    assert_eq!(sessions_of(&fixture).len(), 1);
    // The tab is shown beside the thread.
    until(&mut dom, "the tab", |dom| control(dom, "Search or enter URL").is_some()).await;
    finish(real, fixture, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_integrations_page_adds_renames_defaults_clears_and_removes_browser_profiles() {
    use crate::runtime::transport_tests::{change_control, click_control};
    let (fixture, real) = serving_real().await;
    let (mut dom, _props, state) = ready(&fixture).await;
    until(&mut dom, "the settings", |_| state.peek().config["settings"].is_object()).await;
    state.view().set(View::Integrations);
    until(&mut dom, "the profiles", |dom| control(dom, "Add profile").is_some()).await;
    let profiles = || state.client_settings().peek().browser_profiles.iter().map(|profile| (profile.id.as_str().to_owned(), profile.name.0.as_str().to_owned())).collect::<Vec<_>>();
    assert!(rendered_text(&dom).contains("Default") && control(&dom, "Set Default as default").is_some());
    click_control(&mut dom, "Add profile");
    until(&mut dom, "the profile", |dom| profiles().len() == 1 && control(dom, "Rename New profile").is_some()).await;
    let id = profiles()[0].0.clone();
    change_control(&mut dom, "Rename New profile", "  Work  ");
    until(&mut dom, "the new name", |dom| control(dom, "Rename Work").is_some()).await;
    assert_eq!(profiles(), [(id.clone(), "Work".to_owned())]);
    click_control(&mut dom, "Set Work as default");
    until(&mut dom, "the default", |_| state.client_settings().peek().browser_default_profile_id.as_str() == id).await;

    // Clearing asks first, then the server forgets the profile's storage.
    let storage = real.profiles.join(&id);
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("Cookies"), "x").unwrap();
    click_control(&mut dom, "Clear cookies and cache of Work");
    until(&mut dom, "the question", |dom| control(dom, "Clear data").is_some()).await;
    assert!(storage.exists(), "nothing is deleted before the answer");
    click_control(&mut dom, "Clear data");
    server_side(&mut dom, "the storage to go", || !storage.exists()).await;
    until(&mut dom, "the notice", |dom| rendered_text(dom).contains("Cleared Work\u{2019}s cookies and cache")).await;

    // Removing deletes the data and the profile, and the default goes back to the built-in one.
    std::fs::create_dir_all(&storage).unwrap();
    click_control(&mut dom, "Remove Work");
    until(&mut dom, "the question", |dom| control(dom, "Remove profile").is_some()).await;
    click_control(&mut dom, "Remove profile");
    until(&mut dom, "the profile to go", |_| profiles().is_empty()).await;
    assert!(!storage.exists());
    assert_eq!(state.client_settings().peek().browser_default_profile_id.as_str(), "default");
    finish(real, fixture, dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_empty_tab_lists_the_servers_listening_on_the_environment_and_a_click_opens_one() {
    use t3_server::resource_ports::LocalServer;
    let listening = Arc::new(std::sync::Mutex::new(Vec::<LocalServer>::new()));
    let (fixture, real) = serving_real_finding(Some(super::super::diff_tests::scripted_discovery(listening.clone()))).await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "the launcher", |dom| control(dom, "Open Browser").is_some()).await;
    click_when_enabled(&mut dom, "Open Browser").await;
    until(&mut dom, "the empty tab", |dom| rendered_text(dom).contains("No preview yet")).await;
    // The first viewer of a tab nobody owns controls it, which is what lets a click go to an address.
    until(&mut dom, "the stream", |dom| control(dom, "Release control").is_some()).await;
    // A dev server comes up: the environment's scan finds it, and the tab lists it by port.
    let server = |port: u16, name: &str| LocalServer { host: "localhost".into(), port, url: format!("http://localhost:{port}"), process_name: Some(name.into()), pid: None, terminal: None };
    *listening.lock().unwrap() = vec![server(5173, "vite"), server(3000, "node")];
    until(&mut dom, "the servers", |dom| control(dom, "Open localhost:3000").is_some() && control(dom, "Open localhost:5173").is_some()).await;
    assert!(rendered_text(&dom).contains("Local servers") && rendered_text(&dom).contains("vite") && !rendered_text(&dom).contains("No preview yet"));
    click_when_enabled(&mut dom, "Open localhost:5173").await;
    server_side(&mut dom, "the page", || real.commands_named("Page.navigate").iter().any(|command| command.params["url"] == "http://localhost:5173/")).await;
    finish(real, fixture, dom).await;
}
