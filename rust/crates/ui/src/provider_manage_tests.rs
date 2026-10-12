//! Provider management through the mounted settings page and a real in-process server: refreshing,
//! the version advisory and its update, the managed runtime, and an ACP agent's sessions. The
//! providers are the server tests' fakes: Codex CLIs inside a temp T3 home, a scripted ACP agent, fake
//! update tools found only through a temp search path, and loopback npm and release hosts. Nothing
//! reads the machine's own providers, home directory or network.
#![cfg(unix)]
#[path = "../../server/tests/provider_manage_support/mod.rs"]
#[allow(dead_code)]
mod support;
use super::diff_tests::{until, wait_for_rendered_text};
use super::fixture_server::Server;
use super::keys_tests::key_on;
use super::*;
use crate::runtime::transport_tests::{change_control, click_control, click_when_enabled, control, control_attribute, input_control, rendered_text};
use std::path::Path;
use support::{Harness, Registry, Releases, Route, archive, package, sha256_hex, start, write_executable};

#[derive(Clone)]
struct App {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    address: String,
    token: String,
    probe: Option<crate::RenderProbe>,
}
fn app(props: App) -> Element {
    let state = use_store(|| UiModel { view: View::Providers, ..UiModel::default() });
    crate::themes::use_themes(state);
    if let Some(probe) = props.probe.clone() {
        use_context_provider(|| probe);
    }
    *props.state.borrow_mut() = Some(state);
    let transport = props.transport.clone();
    use_future(move || {
        let (transport, address, token) = (transport.clone(), props.address.clone(), props.token.clone());
        async move { crate::runtime::connect(transport, state, address, token).await }
    });
    rsx! { crate::Application { state, transport: props.transport } }
}

/// A page of providers on a connection with these rights. Dropping it, even while a failed assertion
/// unwinds, stops the services it started.
struct Page {
    support: Option<Harness>,
    server: Option<Server>,
    dom: VirtualDom,
    state: Store<UiModel>,
    props: App,
}
impl Page {
    async fn open(support: Harness, token: &str, probe: Option<crate::RenderProbe>) -> Self {
        let mut server = Server::guard();
        let settings = support.settings.clone();
        server.also(async move { settings.shutdown().await });
        let props = App { state: Rc::new(RefCell::new(None)), transport: TransportHandle::default(), address: format!("http://{}", support.address), token: token.to_owned(), probe };
        let mut dom = VirtualDom::new_with_props(app, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        until(&mut dom, "the provider page", |dom| control(dom, "Refresh provider status").is_some()).await;
        Self { support: Some(support), server: Some(server), dom, state, props }
    }
    fn support(&self) -> &Harness {
        self.support.as_ref().unwrap()
    }
    fn text(&self) -> String {
        rendered_text(&self.dom)
    }
    async fn wait(&mut self, text: &str) {
        wait_for_rendered_text(&mut self.dom, text).await;
    }
    async fn gone(&mut self, text: &str) {
        let text = text.to_owned();
        until(&mut self.dom, &format!("{text} to go"), move |dom| !rendered_text(dom).contains(&text)).await;
    }
    fn click(&mut self, label: &str) {
        click_control(&mut self.dom, label);
    }
    /// Selects a provider in the list once the configuration that lists it has arrived.
    async fn select(&mut self, name: &str) {
        let label = format!("Select provider {name}");
        until(&mut self.dom, &label, |dom| control(dom, &label).is_some()).await;
        click_control(&mut self.dom, &label);
    }
    fn disabled(&self, label: &str) -> bool {
        control_attribute(&self.dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
    }
    fn has(&self, label: &str) -> bool {
        control(&self.dom, label).is_some()
    }
    async fn finish(mut self) {
        drop(std::mem::replace(&mut self.dom, VirtualDom::new(|| rsx! {})));
        self.server.take().unwrap().stop().await;
        self.support.take().unwrap().finish().await;
    }
}

/// A fake Codex CLI reporting the version in the file `version` beside it, installed the way npm lays
/// out a global package: `<prefix>/lib/node_modules/@openai/codex/bin/codex` linked from `<prefix>/bin/codex`.
fn npm_codex(prefix: &Path, version: &str) -> std::path::PathBuf {
    let package_bin = prefix.join("lib/node_modules/@openai/codex/bin");
    std::fs::create_dir_all(&package_bin).unwrap();
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../server/tests/fixtures/provider-manage-codex.py")).unwrap();
    write_executable(&package_bin.join("codex"), &script);
    std::fs::write(package_bin.join("version"), version).unwrap();
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    std::os::unix::fs::symlink("../lib/node_modules/@openai/codex/bin/codex", prefix.join("bin/codex")).unwrap();
    prefix.join("bin/codex")
}

fn codex_instance(binary: &Path, home: &Path) -> Value {
    json!({"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":home}})
}
fn fake_tool(directory: &Path, name: &str, script: &str) {
    write_executable(&directory.join(name), &format!("#!/bin/sh\n{script}\n"));
}
/// A fake `npm` that records its arguments and "installs" `version` into the fake Codex package.
fn fake_npm(directory: &Path, prefix: &Path, version: &str) {
    let log = directory.join("npm.log");
    let version_file = prefix.join("lib/node_modules/@openai/codex/bin/version");
    fake_tool(directory, "npm", &format!("printf '%s\\n' \"$*\" >> '{}'\nprintf '%s' '{version}' > '{}'", log.display(), version_file.display()));
}
fn npm_log(directory: &Path) -> Vec<String> {
    std::fs::read_to_string(directory.join("npm.log")).map(|text| text.lines().map(str::to_owned).collect()).unwrap_or_default()
}

/// A Codex installed by npm at `version`, a loopback registry saying `latest`, and an empty directory
/// for fake update tools.
async fn outdated_codex(version: &'static str, latest: &str) -> (Harness, Registry, std::path::PathBuf, std::path::PathBuf) {
    let registry = Registry::start(Some(latest)).await;
    let url = registry.url.clone();
    let harness = start(
        |base| json!({"codex":codex_instance(&npm_codex(&base.join("npm"), version), &base.join("codex-home"))}),
        move |base, management| {
            std::fs::create_dir_all(base.join("tools-bin")).unwrap();
            let path = base.join("tools-bin").to_string_lossy().into_owned();
            management.configure(|options| {
                options.registry_url = url;
                options.path = Some(path);
            });
        },
    )
    .await;
    let (prefix, tools) = (harness.base.join("npm"), harness.base.join("tools-bin"));
    (harness, registry, prefix, tools)
}

#[tokio::test(flavor = "current_thread")]
async fn a_refresh_finds_an_update_and_running_it_asks_first_shows_the_command_and_runs_once() {
    let (harness, registry, prefix, tools) = outdated_codex("0.156.1", "0.158.0").await;
    fake_npm(&tools, &prefix, "0.158.0");
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Codex").await;
    // The first probe carries no advisory: nothing is known of newer versions until a refresh.
    page.wait("v0.156.1").await;
    assert!(!page.has("Update available \u{2014} view details"));
    assert_eq!(registry.count(), 0);

    page.click("Refresh provider status");
    assert!(page.disabled("Refresh provider status"), "a second refresh waits for the answer");
    page.wait("Update available").await;
    assert_eq!(registry.count(), 1);
    assert!(!page.disabled("Refresh provider status"), "the control comes back with the answer");
    page.click("Update available \u{2014} view details");
    let command = format!("npm install -g --prefix {} --allow-scripts=@openai/codex @openai/codex@latest", prefix.display());
    assert!(page.text().contains(&command), "{}", page.text());
    assert!(page.text().contains("or, update manually using"));
    key_on(&mut page.dom, "Update available", Key::Escape);
    assert!(!page.has("Run provider update"), "Escape closes the popover");
    page.click("Update available \u{2014} view details");

    // Declining runs nothing.
    page.click("Run provider update");
    assert!(page.text().contains("Update Codex?") && page.text().contains(&format!("This runs `{command}` on")), "{}", page.text());
    key_on(&mut page.dom, "Update Codex?", Key::Escape);
    assert!(!page.text().contains("Update Codex?"), "Escape declines");
    assert!(npm_log(&tools).is_empty());

    // Confirming sends it once, and the control stays down until the server has answered.
    page.click("Run provider update");
    page.click("Confirm");
    assert!(page.disabled("Run provider update"), "no second request while one is in flight");
    page.wait("Provider updated. New sessions will use the updated provider.").await;
    page.wait("v0.158.0").await;
    assert_eq!(npm_log(&tools), [format!("install -g --prefix {} --allow-scripts=@openai/codex @openai/codex@latest", prefix.display())]);
    assert_eq!(page.state.peek().error, None);
    page.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_update_shows_the_servers_reason_and_what_the_command_printed() {
    let (harness, _registry, _prefix, tools) = outdated_codex("0.156.1", "0.200.0").await;
    fake_tool(&tools, "npm", "echo 'EACCES: permission denied'\nexit 3");
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Codex").await;
    page.click("Refresh provider status");
    page.wait("Update available").await;
    page.click("Update available \u{2014} view details");
    page.click("Run provider update");
    page.click("Confirm");
    page.wait("Provider update failed").await;
    page.wait("Update command exited with code 3.").await;
    page.wait("EACCES: permission denied").await;
    // The update can be asked for again once the server has answered.
    assert!(!page.disabled("Run provider update"));
    page.finish().await;
}

/// A Codex that signs in on its own has no sign-in stream on the server: asking for one failed with
/// "This provider instance is no longer available." in the app-wide banner on opening Settings.
#[tokio::test(flavor = "current_thread")]
async fn a_provider_that_offers_no_sign_in_has_no_account_section_and_asks_for_no_stream() {
    let (harness, _registry, _prefix, _tools) = outdated_codex("0.156.1", "0.158.0").await;
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Codex").await;
    page.wait("Binary path").await;
    assert!(control(&page.dom, "Provider account").is_none() && !page.text().contains("Sign in on"), "{}", page.text());
    assert_eq!(page.state.peek().error, None);
    assert!(!crate::provider_auth::offered(&json!({"driver":"codex","installed":true})));
    assert!(crate::provider_auth::offered(&json!({"driver":"codex","setup":{"canAuthenticate":true}})));
    assert!(crate::provider_auth::offered(&json!({"driver":"acpRegistry","installed":true})));
    page.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_manage_providers_can_refresh_and_read_the_command_but_not_run_it() {
    let (harness, registry, _prefix, tools) = outdated_codex("0.156.1", "0.158.0").await;
    let token = harness.reader.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Codex").await;
    assert!(page.text().contains("Provider settings are read-only."));
    assert!(!page.has("Add provider"), "adding an instance needs the right to manage");
    page.click("Refresh provider status");
    page.wait("Update available").await;
    assert_eq!(registry.count(), 1);
    page.click("Update available \u{2014} view details");
    assert!(page.text().contains("npm install -g --prefix"));
    assert!(!page.has("Run provider update"));
    assert!(npm_log(&tools).is_empty());
    page.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn adding_an_instance_derives_its_id_checks_it_and_creates_it_through_one_settings_change() {
    let (harness, _registry, _prefix, _tools) = outdated_codex("0.156.1", "0.158.0").await;
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    // The instances that exist are known once the configuration has arrived.
    page.select("Codex").await;
    page.click("Add provider");
    key_on(&mut page.dom, "Add provider instance", Key::Escape);
    assert!(!page.has("Add instance"), "Escape closes the dialog");
    page.click("Add provider");
    change_control(&mut page.dom, "Provider driver", "claudeAgent");
    input_control(&mut page.dom, "Label", "Work Laptop");
    assert_eq!(control(&page.dom, "Instance ID").unwrap().1.as_deref(), Some("claudeAgent_work_laptop"));
    input_control(&mut page.dom, "Instance ID", "codex");
    page.click("Add instance");
    assert!(page.text().contains("An instance named 'codex' already exists."), "{}", page.text());
    input_control(&mut page.dom, "Instance ID", "claudeAgent_work_laptop");
    page.click("Add instance");
    assert!(page.disabled("Add instance"), "one request while it is in flight");
    page.wait("Provider instance added").await;
    page.wait("Claude instance 'claudeAgent_work_laptop' was added.").await;
    let settings = page.support().settings.snapshot().await.unwrap();
    let created = &serde_json::to_value(&settings).unwrap()["providerInstances"]["claudeAgent_work_laptop"];
    assert_eq!((created["driver"].as_str(), created["displayName"].as_str(), created["enabled"].as_bool()), (Some("claudeAgent"), Some("Work Laptop"), Some(true)));
    page.select("Work Laptop").await;
    page.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_local_acp_command_takes_literal_arguments_one_per_row_in_order() {
    let harness = start(|base| json!({"local":{"driver":"acpRegistry","enabled":false,"displayName":"Local agent","config":{"source":"local","commandPath":base.join("agent"),"commandArgs":["--profile","acp"]}}}), |_, _| {}).await;
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Local agent").await;
    assert_eq!(control(&page.dom, "Argument 1").unwrap().1.as_deref(), Some("--profile"));
    page.click("Add argument");
    until(&mut page.dom, "the new row", |dom| control(dom, "Argument 3").is_some()).await;
    input_control(&mut page.dom, "Argument 3", " literal $(value) ; ");
    blur(&mut page.dom, "Argument 3");
    until(&mut page.dom, "the argument saved", |_| page.state.peek().config["settings"]["providerInstances"]["local"]["config"]["commandArgs"] == json!(["--profile", "acp", " literal $(value) ; "])).await;
    page.click("Remove argument 1");
    until(&mut page.dom, "the argument removed", |_| page.state.peek().config["settings"]["providerInstances"]["local"]["config"]["commandArgs"] == json!(["acp", " literal $(value) ; "])).await;
    page.finish().await;
}

fn blur(dom: &mut VirtualDom, label: &str) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    dom.runtime().handle_event(
        "blur",
        dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(dioxus_html::SerializedFocusData::default()))) as Rc<dyn std::any::Any>, true),
        element,
    );
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}

// ---- the managed runtime ----------------------------------------------------------------------

const VERSION: &str = "0.156.1";
const TARGET: &str = "test-target";

/// A managed Codex over a loopback release host serving `/package.tgz`; the machine's PATH is a temp directory. `issuer` is the
/// loopback stand-in for OpenAI that its ChatGPT sign-in uses (the discovery and model list addresses); without one the server
/// keeps no ChatGPT sign-in at all.
async fn managed_codex(good: &[u8], gated: Option<(usize, std::sync::Arc<tokio::sync::Notify>, std::sync::Arc<tokio::sync::Notify>)>, issuer: Option<(String, String)>) -> (Harness, Releases) {
    let releases = Releases::start().await;
    let url = match gated {
        Some((first, started, release)) => releases.serve("/package.tgz", Route::Gated { body: good.to_vec(), first, started, release }),
        None => releases.serve("/package.tgz", Route::Body(good.to_vec())),
    };
    let asset = t3_server::provider_install::ReleaseAsset { version: VERSION.into(), target: TARGET.into(), url, sha256: sha256_hex(good), archive_bytes: good.len() as u64 };
    let harness = start(
        |base| json!({"codex":{"driver":"codex","enabled":true,"config":{"setupMode":"managed","homePath":base.join("codex-home")}}}),
        move |base, management| {
            let path_dir = base.join("path");
            std::fs::create_dir_all(&path_dir).unwrap();
            let path = path_dir.to_string_lossy().into_owned();
            management.configure(|options| {
                options.codex_release = Some(Some(asset));
                options.codex_path = Some(path);
            });
            if let Some((discovery_url, models_url)) = issuer {
                // The harness keeps the server's state under `<home>/userdata`.
                let environment = t3_server::codex_chatgpt_rpc::Environment { environment_id: "manage-fixture".into(), default_return_url: None, discovery_url: Some(discovery_url), resource: None, models_url: Some(models_url), http: reqwest::Client::new() };
                management.set_chatgpt(t3_server::codex_managed::ChatGptHost { environment: std::sync::Arc::new(environment), state_dir: base.join("userdata") });
            }
        },
    )
    .await;
    (harness, releases)
}

/// The address of the sign-in page the server offers, read from a subscription of the test's own (the page never shows it): the
/// test is the person's browser.
async fn sign_in_page(socket: &mut support::Socket) -> String {
    loop {
        let message = support::next_while(socket, "the ChatGPT sign-in page address").await;
        assert_ne!(message["_tag"], "Exit", "the sign-in stream ended: {message}");
        if message["_tag"] != "Chunk" {
            continue;
        }
        support::send(socket, json!({"_tag":"Ack","requestId":1})).await;
        for state in message["values"].as_array().unwrap() {
            if let (Some("waiting"), Some(url)) = (state["phase"].as_str(), state["interaction"]["url"].as_str()) {
                return url.to_owned();
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_managed_runtime_installs_with_progress_cancels_retries_and_signs_in_with_chatgpt() {
    use std::sync::Arc;
    let good = archive(&package(VERSION, TARGET, VERSION));
    let issuer = super::fake_issuer::Issuer::start().await;
    let (started, release) = (Arc::new(tokio::sync::Notify::new()), Arc::new(tokio::sync::Notify::new()));
    let (harness, releases) = managed_codex(&good, Some((good.len() / 2, started.clone(), release.clone())), Some((issuer.discovery_url(), issuer.models_url()))).await;
    let managed = harness.base.join("tools/codex");
    let token = harness.manager.clone();
    let mut socket = harness.connect(&token).await;
    let mut page = Page::open(harness, &token, None).await;
    page.select("Codex").await;
    page.wait("T3 Code downloads and manages Codex for you.").await;
    assert!(page.has("Install Codex"));

    click_when_enabled(&mut page.dom, "Install Codex").await;
    assert!(page.disabled("Install Codex"), "a second start waits for the answer");
    started.notified().await;
    until(&mut page.dom, "the download", |dom| rendered_text(dom).contains("Downloading ") && control(dom, "Codex download").is_some()).await;
    assert!(page.text().contains(" MB"), "{}", page.text());
    // Cancelling asks the server to stop the operation it is running, once.
    until(&mut page.dom, "cancel offered", |dom| rendered_text(dom).contains("Cancel installation")).await;
    click_when_enabled(&mut page.dom, "Cancel installation").await;
    page.wait("Installation cancelled. The previous runtime is unchanged.").await;
    page.wait("Retry installation").await;
    assert!(!managed.join("0.156.1").exists());
    release.notify_one();

    // Retrying on a host that now serves the whole archive installs it.
    releases.serve("/package.tgz", Route::Body(good.clone()));
    click_when_enabled(&mut page.dom, "Retry installation").await;
    page.wait("Managed by T3 Code \u{b7} v0.156.1.").await;
    assert!(managed.join("0.156.1").exists());
    assert!(page.has("Remove downloaded runtime") && page.has("Reinstall Codex"));
    // Installed, the runtime still has no account: the server says so and offers the sign-in it now has (a managed Codex signs
    // in with ChatGPT on the server itself).
    page.wait("Sign in with ChatGPT to use Codex.").await;
    assert!(!page.text().contains("ChatGPT sign-in is not available on this server."), "{}", page.text());
    assert!(!page.text().contains("This provider instance is no longer available."), "no app-wide error from a sign-in stream this provider does not have");
    assert_eq!(page.state.peek().error, None);

    support::send(&mut socket, json!({"_tag":"Request","id":1,"tag":"provider.auth.subscribe","payload":{"instanceId":"codex"},"headers":[]})).await;
    click_when_enabled(&mut page.dom, "Sign in").await;
    page.wait("Finish signing in in your browser.").await;
    assert!(page.has("Cancel sign-in") && page.has("Sign-in callback URL"), "the redirect can be pasted if the page does not return");
    // The person's browser goes to the issuer, which sends it back to the server's own listener with a code.
    let url = sign_in_page(&mut socket).await;
    let returned = reqwest::get(&url).await.unwrap();
    assert!(returned.status().is_success(), "{}", returned.status());
    let request = issuer.authorized.lock().unwrap().last().cloned().unwrap();
    assert_eq!((request["response_type"].as_str(), request["code_challenge_method"].as_str()), ("code", "S256"));
    page.wait("Signed in as").await;
    // The signed-in email is hidden behind a placeholder until asked for, and no credential or sign-in secret is ever drawn.
    let text = page.text();
    assert!(!text.contains("Signed in as a@example.com"), "{text}");
    for secret in ["access-1", "refresh-1", request["state"].as_str(), request["code_challenge"].as_str(), "good-code"] {
        assert!(!text.contains(secret), "{secret} is on the page");
    }
    page.click("Toggle account email visibility");
    assert!(page.text().contains("a@example.com"), "{}", page.text());
    assert!(page.text().contains("Signed in as a@example.com"), "{}", page.text());
    drop(socket);
    page.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn removing_a_managed_runtime_asks_in_the_originals_words_and_escape_declines() {
    let good = archive(&package(VERSION, TARGET, VERSION));
    let (harness, _releases) = managed_codex(&good, None, None).await;
    let managed = harness.base.join("tools/codex");
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Codex").await;
    click_when_enabled(&mut page.dom, "Install Codex").await;
    page.wait("Managed by T3 Code").await;
    page.click("Remove downloaded runtime");
    assert!(page.text().contains("Remove the downloaded Codex runtime from Fixture? Thread history is kept."), "{}", page.text());
    key_on(&mut page.dom, "Remove the downloaded Codex runtime from Fixture? Thread history is kept.", Key::Escape);
    assert!(managed.join("0.156.1").exists(), "declining removes nothing");
    page.click("Remove downloaded runtime");
    page.click("Confirm");
    page.wait("T3 Code downloads and manages Codex for you.").await;
    assert!(!managed.join("0.156.1").exists());
    page.finish().await;
}

// ---- ACP registry agents ----------------------------------------------------------------------

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../server/tests/fixtures/acp-session-manager.py");

/// A scripted ACP agent in a project, each management call of which starts the agent afresh.
async fn acp_agent(scenario: &str) -> Harness {
    use t3_server::project::ProjectService;
    let scenario = scenario.to_owned();
    let harness = start(
        move |base| json!({"agent":{"driver":"acpRegistry","enabled":true,"displayName":"Session agent","environment":[{"name":"MANAGE_SCENARIO","value":scenario}],
            "config":{"source":"local","commandPath":AGENT,"commandArgs":[base.join("agent.log")]}}}),
        |_, _| {},
    )
    .await;
    let root = harness.base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    ProjectService::new(harness.store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":root}), chrono::Utc::now()).unwrap();
    harness
}

#[tokio::test(flavor = "current_thread")]
async fn an_acp_agents_sessions_list_by_page_import_and_delete_after_asking() {
    let harness = acp_agent("normal").await;
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Session agent").await;
    until(&mut page.dom, "the sessions section", |dom| control(dom, "List ACP sessions").is_some()).await;
    assert!(!page.text().contains("First session"), "nothing is listed until asked");
    page.click("List ACP sessions");
    assert!(page.disabled("List ACP sessions"), "one listing at a time");
    page.wait("First session").await;
    assert!(page.has("Load more ACP sessions"));
    page.click("Load more ACP sessions");
    until(&mut page.dom, "the second page", |dom| control(dom, "Import ACP session s-2").is_some()).await;
    assert!(!page.has("Load more ACP sessions"), "the last page has no cursor");

    // Importing reports what the server says; a refusal leaves the action where it was.
    page.click("Import ACP session s-1");
    assert!(page.disabled("Import ACP session s-1") && page.disabled("Import ACP session s-2"), "one import in flight");
    until(&mut page.dom, "the import answered", |dom| rendered_text(dom).contains("Imported") || rendered_text(dom).contains("Could not import ACP session")).await;
    if page.text().contains("Could not import ACP session") {
        assert!(!page.disabled("Import ACP session s-1"), "a failed import can be tried again");
    }

    // Deleting asks first, in the original's words, and Escape declines.
    page.click("Delete ACP session s-2");
    assert!(page.text().contains("Permanently delete native ACP session \"s-2\"?"), "{}", page.text());
    key_on(&mut page.dom, "Permanently delete native ACP session \"s-2\"?", Key::Escape);
    assert!(page.has("Delete ACP session s-2"));
    page.click("Delete ACP session s-2");
    page.click("Confirm");
    page.gone("Import ACP session s-2").await;
    page.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_acp_agents_providers_save_validate_headers_and_disable_after_asking_and_it_logs_out() {
    let harness = acp_agent("normal").await;
    let token = harness.manager.clone();
    let mut page = Page::open(harness, &token, None).await;
    page.select("Session agent").await;
    until(&mut page.dom, "the providers section", |dom| control(dom, "List ACP providers").is_some()).await;
    page.click("List ACP providers");
    until(&mut page.dom, "the providers", |dom| control(dom, "openai base URL").is_some()).await;
    assert_eq!(control(&page.dom, "openai base URL").unwrap().1.as_deref(), Some("https://api.example.test/v1"));
    // Headers are a JSON object of strings, and the server never gets anything else.
    input_control(&mut page.dom, "openai write-only headers JSON", "[1]");
    page.click("Save openai");
    page.wait("Invalid provider headers").await;
    page.wait("Headers must be a JSON object with string values.").await;
    input_control(&mut page.dom, "openai write-only headers JSON", "");
    page.click("Save openai");
    page.wait("ACP provider configured").await;
    // A required provider cannot be disabled; an optional one asks first.
    assert!(!page.has("Disable core"));
    page.click("Disable openai");
    assert!(page.text().contains("Disable ACP provider \"openai\"?"));
    key_on(&mut page.dom, "Disable ACP provider \"openai\"?", Key::Escape);
    page.click("Disable openai");
    page.click("Confirm");
    page.wait("ACP provider disabled").await;
    if page.has("Log out of ACP agent") {
        page.click("Log out of ACP agent");
        page.wait("Logged out of ACP agent").await;
    }
    page.finish().await;
}
