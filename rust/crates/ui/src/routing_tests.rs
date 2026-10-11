//! The address bar through the mounted application and the real native server. The browser is a fake
//! document that stands in for `routing_abi.js`: it holds the addresses the person "typed" or went back to,
//! and records what the app asked the page to write. What a thread, a settings page and the start
//! screen are addressed as is `t3_client::route`'s; here it is that the view follows the address and
//! the address follows the view, with nothing rebuilt that did not change.
use super::diff_tests::{Fixture, Harness, git, open_thread, pump, rendered_text, seed, turn, until};
use crate::runtime::transport_tests::{control, control_attribute, input_control};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::View;
use std::{cell::RefCell, collections::VecDeque, rc::Rc, task::{Context, Poll, Waker}};
use t3_contracts::AuthEnvironmentScope;

#[derive(Default)]
pub(crate) struct Browser {
    /// Addresses the page reports to the app, oldest first.
    incoming: VecDeque<Value>,
    waker: Option<Waker>,
    /// What the app asked the page to do.
    pub(crate) sent: Vec<Value>,
}
impl Browser {
    pub(crate) fn go(&mut self, address: &str) {
        self.incoming.push_back(json!({"type":"location","path":address}));
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}
struct Page {
    browser: Rc<RefCell<Browser>>,
    routing: bool,
}
impl document::Evaluator for Page {
    fn send(&self, value: Value) -> Result<(), document::EvalError> {
        if self.routing {
            self.browser.borrow_mut().sent.push(value);
        }
        Ok(())
    }
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Value, document::EvalError>> {
        if !self.routing {
            return Poll::Pending;
        }
        let mut browser = self.browser.borrow_mut();
        match browser.incoming.pop_front() {
            Some(message) => Poll::Ready(Ok(message)),
            None => {
                browser.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
    fn poll_join(&mut self, _: &mut Context<'_>) -> Poll<Result<Value, document::EvalError>> {
        Poll::Pending
    }
}
pub(crate) struct BrowserDocument {
    pub(crate) owner: dioxus::signals::Owner,
    pub(crate) browser: Rc<RefCell<Browser>>,
}
impl document::Document for BrowserDocument {
    fn eval(&self, script: String) -> document::Eval {
        let routing = script.contains("window.addEventListener(\"popstate\"");
        document::Eval::new(self.owner.insert(Box::new(Page { browser: self.browser.clone(), routing }) as Box<dyn document::Evaluator>))
    }
}

pub(crate) fn last_push(browser: &Rc<RefCell<Browser>>) -> Option<String> {
    browser.borrow().sent.iter().rev().find(|message| message["type"] == "push").and_then(|message| message["path"].as_str()).map(str::to_owned)
}
fn active(state: Store<UiModel>) -> Option<String> {
    state.peek().active_thread.clone()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_address_follows_the_view_and_the_view_follows_the_address() {
    let fixture = Fixture::start(true).await;
    let browser = Rc::new(RefCell::new(Browser::default()));
    let probe = crate::RenderProbe::default();
    let (mut dom, props) = fixture.client_with(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], {
        let browser = browser.clone();
        let probe = probe.clone();
        move |dom| {
            dom.provide_root_context(crate::routing::Enabled);
            dom.provide_root_context(probe);
            dom.provide_root_context(Rc::new(BrowserDocument { owner: Default::default(), browser }) as Rc<dyn document::Document>);
        }
    });
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let environment = state.peek().destination.as_ref().unwrap().to_string();

    // Opening the app at a thread's address opens that thread, and the address is not written over.
    browser.borrow_mut().go(&format!("/{environment}/thread-b"));
    until(&mut dom, "the thread named by the address", |_| active(state).as_deref() == Some("thread-b") && state.peek().thread.synchronized).await;
    until(&mut dom, "the address kept", |_| last_push(&browser).is_some()).await;
    assert_eq!(last_push(&browser).unwrap(), format!("/{environment}/thread-b"));

    // Opening another thread writes its address.
    dom.in_scope(ScopeId::APP, || crate::runtime::select_thread(&props.transport, state, "thread-a".into()));
    until(&mut dom, "thread-a's address", |_| last_push(&browser).as_deref() == Some(&format!("/{environment}/thread-a"))).await;

    // A settings page has its own address, and leaving it for the chat writes the thread's again.
    dom.in_scope(ScopeId::APP, || state.view().set(View::Providers));
    until(&mut dom, "the providers address", |_| last_push(&browser).as_deref() == Some("/settings/providers")).await;
    dom.in_scope(ScopeId::APP, || state.view().set(View::Connections));
    until(&mut dom, "the connections address", |_| last_push(&browser).as_deref() == Some("/settings/connections")).await;
    dom.in_scope(ScopeId::APP, || state.view().set(View::Chat));
    until(&mut dom, "the thread's address again", |_| last_push(&browser).as_deref() == Some(&format!("/{environment}/thread-a"))).await;

    // Back and forward: an address the page reports shows that view.
    browser.borrow_mut().go("/settings/providers");
    until(&mut dom, "the providers page", |_| *state.view().peek() == View::Providers).await;
    browser.borrow_mut().go(&format!("/{environment}/thread-b"));
    until(&mut dom, "thread-b again", |_| active(state).as_deref() == Some("thread-b") && *state.view().peek() == View::Chat).await;
    browser.borrow_mut().go("/");
    until(&mut dom, "the start screen", |_| active(state).is_none() && *state.view().peek() == View::Chat).await;
    until(&mut dom, "the start address", |_| last_push(&browser).as_deref() == Some("/")).await;

    // The same address as the one shown changes nothing, and the conversation is not rebuilt for it.
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "thread-a's address", |_| last_push(&browser).as_deref() == Some(&format!("/{environment}/thread-a"))).await;
    let before = (probe.application.get(), probe.timeline.get(), probe.composer.get());
    browser.borrow_mut().go(&format!("/{environment}/thread-a"));
    until(&mut dom, "the address read", |_| browser.borrow().incoming.is_empty()).await;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!((probe.application.get(), probe.timeline.get(), probe.composer.get()), before);
    assert_eq!(active(state).as_deref(), Some("thread-a"));

    // A thread this server does not have, or one of another environment, shows the start screen with a notice
    // and the address says so.
    for missing in [format!("/{environment}/no-such-thread"), "/another-environment/thread-a".to_owned()] {
        state.notices().write().clear();
        browser.borrow_mut().go(&missing);
        until(&mut dom, "the start screen after a missing thread", |_| active(state).is_none() && state.notices().peek().iter().any(|notice| notice.title == "Thread not found")).await;
        until(&mut dom, "the address rewritten", |_| browser.borrow().sent.iter().rev().take(2).any(|message| message["type"] == "replace" && message["path"] == "/")).await;
        open_thread(&mut dom, &props, state, "thread-a").await;
    }
    // An address that names nothing shows the start screen without a notice.
    state.notices().write().clear();
    browser.borrow_mut().go("/settings/no-such-page");
    until(&mut dom, "the start screen for nothing", |_| active(state).is_none()).await;
    assert!(state.notices().peek().is_empty());
    fixture.finish(dom).await;
}

/// A page opened at a settings address shows it at once, before the server has answered anything.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_settings_address_needs_no_server_to_show() {
    let fixture = Fixture::start(false).await;
    let browser = Rc::new(RefCell::new(Browser::default()));
    browser.borrow_mut().go("/settings/connections");
    let (mut dom, props) = fixture.client_with(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead], {
        let browser = browser.clone();
        move |dom| {
            dom.provide_root_context(crate::routing::Enabled);
            dom.provide_root_context(Rc::new(BrowserDocument { owner: Default::default(), browser }) as Rc<dyn document::Document>);
        }
    });
    let state = props.state.borrow().unwrap();
    until(&mut dom, "the connections page", |_| *state.view().peek() == View::Connections).await;
    fixture.finish(dom).await;
}

/// A client whose page already has `address` in its bar when the application mounts, so the page that names is
/// shown before the connection is up: the way a cold load or a reload of that address starts.
pub(crate) fn cold(fixture: &Fixture, address: &str, scopes: &[AuthEnvironmentScope], setup: impl FnOnce(&mut VirtualDom)) -> (VirtualDom, Harness, Store<UiModel>, Rc<RefCell<Browser>>) {
    let browser = Rc::new(RefCell::new(Browser::default()));
    browser.borrow_mut().go(address);
    let (dom, props) = fixture.client_with(Gate::default(), scopes, {
        let browser = browser.clone();
        move |dom| {
            dom.provide_root_context(crate::routing::Enabled);
            dom.provide_root_context(Rc::new(BrowserDocument { owner: Default::default(), browser }) as Rc<dyn document::Document>);
            setup(dom);
        }
    });
    let state = props.state.borrow().unwrap();
    (dom, props, state, browser)
}

/// Every page that reads or follows something of its own, opened by its address before the server has said
/// anything: each must show what the server says once the connection is ready, without being opened again.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_page_opened_by_its_address_before_the_connection_is_ready_shows_its_data_once_it_is() {
    let fixture = Fixture::start(false).await;
    let scopes = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite, AuthEnvironmentScope::ProvidersManage, AuthEnvironmentScope::DiagnosticsRead, AuthEnvironmentScope::SourceControlWrite];
    type Shown = fn(&VirtualDom, Store<UiModel>) -> bool;
    let pages: [(&str, View, &str, Shown); 8] = [
        ("/settings/providers", View::Providers, "the provider list", |dom, _| control(dom, "Refresh provider status").is_some()),
        ("/settings/keybindings", View::Keybindings, "the keybindings", |dom, _| control(dom, "Search keybindings").is_some()),
        ("/settings/scheduled-tasks", View::ScheduledTasks, "the task list", |dom, _| rendered_text(dom).contains("No scheduled tasks")),
        ("/settings/source-control", View::SourceControl, "the scan", |dom, _| rendered_text(dom).contains("Source Control Providers")),
        ("/settings/text-generation", View::TextGeneration, "the text generation choices", |dom, _| control(dom, "Text generation scope").is_some()),
        ("/settings/general", View::General, "the general settings", |dom, _| control(dom, "Auto-resume limited threads").is_some()),
        ("/settings/connections", View::Connections, "the connection", |dom, _| rendered_text(dom).matches("Connected").count() >= 2),
        // Limits is the page's first view: the read of the providers is what it waits for.
        ("/usage", View::Usage, "the limits read", |_, state| state.usage().peek().limits_refresh_after.is_some()),
    ];
    let mut last = None;
    for (address, view, what, shown) in pages {
        let (mut dom, _props, state, _browser) = cold(&fixture, address, &scopes, |_| {});
        until(&mut dom, address, |_| *state.view().peek() == view).await;
        assert!(!state.peek().shell.synchronized, "{address} was opened before the server had said anything");
        until(&mut dom, what, |dom| shown(dom, state)).await;
        assert_eq!(state.error().peek().clone(), None, "{address}");
        last = Some(dom);
    }
    fixture.finish(last.unwrap()).await;
}

/// A thread opened by its address waits for the server's list of threads and then shows everything a thread
/// shows: its conversation, its branch in the header, and a diff panel that was open.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_thread_opened_by_its_address_before_the_connection_is_ready_shows_its_conversation_branch_and_diff_once_it_is() {
    let fixture = Fixture::start(true).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    git(&fixture.workspace, &["checkout", "-q", "-b", "feature/x"]);
    let scopes = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead];
    // A first visit gives thread-a a conversation, then leaves.
    let (mut first, props) = fixture.client(Gate::default(), &scopes);
    let state = seed(&mut first, &props, &fixture.workspace).await;
    open_thread(&mut first, &props, state, "thread-a").await;
    turn(&mut first, &props, state, 1).await;
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    drop(first);
    fixture.write("README.md", "hello\nuncommitted\n");

    // The reload: the address is in the bar, the diff panel is open, and nothing is known yet.
    let (mut dom, _props, state, _browser) = cold(&fixture, &format!("/{environment}/thread-a"), &scopes, |_| {});
    state.right_panel().write().panels.open(&format!("{environment}:thread-a"), t3_client::right_panel::Kind::Diff);
    assert!(state.peek().active_thread.is_none() && !state.peek().shell.synchronized);
    until(&mut dom, "the thread named by the address", |_| state.peek().active_thread.as_deref() == Some("thread-a") && state.peek().thread.synchronized).await;
    until(&mut dom, "the conversation", |dom| rendered_text(dom).contains("Fixture response")).await;
    until(&mut dom, "the branch in the header", |dom| rendered_text(dom).contains("feature/x")).await;
    until(&mut dom, "the diff the open panel reads", |dom| control(dom, "Expand README.md").is_some()).await;
    assert_eq!(state.error().peek().clone(), None);
    fixture.finish(dom).await;
}

/// What a switch rebuilt: the application, the conversation, the composer and the conversation's rows, and the sidebar's.
type Built = (usize, usize, usize, usize, usize);
fn built(probe: &crate::RenderProbe) -> Built {
    (probe.application.get(), probe.timeline.get(), probe.composer.get(), probe.item.get(), probe.sidebar_row.get())
}
fn spent(before: Built, after: Built) -> Built {
    (after.0 - before.0, after.1 - before.1, after.2 - before.2, after.3 - before.3, after.4 - before.4)
}

/// Going to a thread by its address (typed, back or forward) is the same switch as clicking it in the sidebar:
/// the router adds no rebuild of the application, the conversation, the composer or the rows.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn switching_threads_by_address_rebuilds_no_more_than_switching_them_from_the_sidebar() {
    let fixture = Fixture::start(false).await;
    let browser = Rc::new(RefCell::new(Browser::default()));
    let probe = crate::RenderProbe::default();
    let (mut dom, props) = fixture.client_with(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], {
        let (browser, probe) = (browser.clone(), probe.clone());
        move |dom| {
            dom.provide_root_context(crate::routing::Enabled);
            dom.provide_root_context(probe);
            dom.provide_root_context(Rc::new(BrowserDocument { owner: Default::default(), browser }) as Rc<dyn document::Document>);
        }
    });
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    // The page was opened at the start screen, which is what lets the app write addresses.
    browser.borrow_mut().go("/");
    // The router reads it at once and changes nothing the page would draw, so nothing wakes the page to say so.
    pump(&mut dom).await;
    assert!(browser.borrow().incoming.is_empty(), "the router read the start address");
    // Each thread has a conversation of its own, and has been visited since its turn ended.
    for thread in ["thread-a", "thread-b"] {
        open_thread(&mut dom, &props, state, thread).await;
        turn(&mut dom, &props, state, 1).await;
        until(&mut dom, "the visit", |_| state.peek().shell.snapshot.as_ref().unwrap().threads.iter().all(|thread| !t3_client::sidebar::has_unseen_completion(thread))).await;
    }
    let address = |thread: &str| format!("/{environment}/{thread}");
    let shown = |thread: &'static str| move |_: &VirtualDom| active(state).as_deref() == Some(thread) && state.peek().thread.synchronized && state.peek().thread.projection.as_ref().is_some_and(|projection| projection["thread"]["id"] == thread);

    // The sidebar's switches, each way and twice (the first of each is the one that opens the thread's place in the sidebar), as the measure.
    let (mut sidebar, mut by_address) = (Vec::new(), Vec::new());
    // How many events the server delivered to the shell during each window. The composer is built again for each, and whether a
    // switch's own visit stamp lands inside its window or just after it is a matter of timing.
    let shell_sequence = || state.peek().shell.snapshot.as_ref().map_or(0, |snapshot| snapshot.snapshot_sequence);
    let (mut sidebar_events, mut address_events) = (Vec::new(), Vec::new());
    for thread in ["thread-a", "thread-b", "thread-a", "thread-b"] {
        let (start, first_event) = (built(&probe), shell_sequence());
        dom.in_scope(ScopeId::APP, || crate::runtime::select_thread(&props.transport, state, thread.into()));
        until(&mut dom, "the sidebar's switch", shown(thread)).await;
        until(&mut dom, "its address", |_| last_push(&browser) == Some(address(thread))).await;
        sidebar.push(spent(start, built(&probe)));
        sidebar_events.push(shell_sequence() - first_event);
    }
    // Back and forward by address, from b: to a, to b again (back), to a (back), to b (forward).
    for thread in ["thread-a", "thread-b", "thread-a", "thread-b"] {
        let (start, first_event) = (built(&probe), shell_sequence());
        browser.borrow_mut().go(&address(thread));
        until(&mut dom, "the address's switch", shown(thread)).await;
        until(&mut dom, "its address kept", |_| last_push(&browser) == Some(address(thread))).await;
        by_address.push(spent(start, built(&probe)));
        address_events.push(shell_sequence() - first_event);
    }
    // The budget is the most any of the sidebar's own switches cost, per measure: which switch pays for an extra
    // pass (the first visit of a place, or a status that lands during it) depends on timing, and an address
    // may not cost more than the sidebar's worst.
    let worst = sidebar.iter().fold(sidebar[0], |most, got| (most.0.max(got.0), most.1.max(got.1), most.2.max(got.2), most.3.max(got.3), most.4.max(got.4)));
    // The conversation, the composer and the items are exact. The application and the rows may each take one pass more
    // than the budget: a status or a timer that lands during one window is not something the address caused.
    // The composer is built once for a switch, and once more for each event the server delivered to the shell meanwhile (a
    // switch's own visit stamp is one): the cheapest sidebar switch is what a switch costs, an event is what a window adds.
    let switch_cost = sidebar.iter().map(|got| got.2).min().unwrap();
    let within = |got: Built, budget: Built, events: u64| got.0 <= budget.0 + 1 && got.1 <= budget.1 && got.2 as u64 <= switch_cost as u64 + events && got.3 <= budget.3 && got.4 <= budget.4 + 1;
    assert!(sidebar[2].1 >= 1 && sidebar[3].1 >= 1, "a switch builds the conversation: {sidebar:?}");
    assert!((0..4).all(|index| within(by_address[index], worst, address_events[index])), "an address rebuilt more than the sidebar did: {by_address:?} ({address_events:?} shell events) against {sidebar:?} ({sidebar_events:?})");
    assert_eq!(state.error().peek().clone(), None);
    fixture.finish(dom).await;
}

/// A settings page replaces the conversation while it is open, and the conversation is back the way it was left:
/// the place the person was reading at, and the words they had not sent. Neither is rebuilt for the page.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn opening_and_closing_a_settings_page_keeps_the_draft_and_the_place_in_the_conversation() {
    let fixture = Fixture::start(false).await;
    let browser = Rc::new(RefCell::new(Browser::default()));
    let probe = crate::RenderProbe::default();
    let positions = crate::timeline_scroll::Positions::default();
    let (mut dom, props) = fixture.client_with(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], {
        let (browser, probe, positions) = (browser.clone(), probe.clone(), positions.clone());
        move |dom| {
            dom.provide_root_context(crate::routing::Enabled);
            dom.provide_root_context(probe);
            dom.provide_root_context(positions);
            dom.provide_root_context(Rc::new(BrowserDocument { owner: Default::default(), browser }) as Rc<dyn document::Document>);
        }
    });
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    // The reader had scrolled up from the end of thread-a before: that place is what the cache remembers.
    let owner = serde_json::to_string(&(&environment, "thread-a")).unwrap();
    positions.remember_reading(&owner);
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert!(control(&dom, "Scroll to end").is_some(), "the conversation opens where the reader was");
    input_control(&mut dom, "Message", "words not sent yet");
    assert_eq!(state.peek().draft, "words not sent yet");
    let value = |dom: &VirtualDom| control_attribute(dom, "Message", "value").and_then(|(_, value)| value);
    assert_eq!(value(&dom).as_deref(), Some("words not sent yet"));

    // To a settings page and back, by the address bar and by the page's own way back.
    for back in [Back::Address, Back::View] {
        let before = built(&probe);
        browser.borrow_mut().go("/settings/providers");
        until(&mut dom, "the page", |_| *state.view().peek() == View::Providers).await;
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!(spent(before, built(&probe)).1, 0, "the page does not build the conversation");
        assert!(control(&dom, "Scroll to end").is_none() && control(&dom, "Message").is_none(), "the conversation is not behind the page");
        let away = built(&probe);
        match back {
            Back::Address => browser.borrow_mut().go(&format!("/{environment}/thread-a")),
            Back::View => dom.in_scope(ScopeId::APP, || state.view().set(View::Chat)),
        }
        until(&mut dom, "the conversation again", |dom| *state.view().peek() == View::Chat && state.peek().thread.synchronized && control(dom, "Message").is_some()).await;
        assert_eq!(spent(away, built(&probe)).1, 1, "the conversation is built once on the way back, by {back:?}");
        assert!(control(&dom, "Scroll to end").is_some(), "the reader's place came back");
        assert!(positions.recalls(&owner));
        assert_eq!(state.peek().draft, "words not sent yet");
        assert_eq!(value(&dom).as_deref(), Some("words not sent yet"), "the draft is in the box again");
        assert_eq!(state.peek().active_thread.as_deref(), Some("thread-a"));
    }
    assert_eq!(state.error().peek().clone(), None);
    fixture.finish(dom).await;
}
#[derive(Debug, Clone, Copy)]
enum Back {
    Address,
    View,
}
