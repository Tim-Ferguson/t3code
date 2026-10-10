//! The address bar through the mounted application and the real native server. The browser is a fake
//! document that stands in for `routing_abi.js`: it holds the addresses the person "typed" or went back to,
//! and records what the app asked the page to write. What a thread, a settings page and the start
//! screen are addressed as is `t3_client::route`'s; here it is that the view follows the address and
//! the address follows the view, with nothing rebuilt that did not change.
use super::diff_tests::{Fixture, open_thread, seed, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::View;
use std::{cell::RefCell, collections::VecDeque, rc::Rc, task::{Context, Poll, Waker}};
use t3_contracts::AuthEnvironmentScope;

#[derive(Default)]
struct Browser {
    /// Addresses the page reports to the app, oldest first.
    incoming: VecDeque<Value>,
    waker: Option<Waker>,
    /// What the app asked the page to do.
    sent: Vec<Value>,
}
impl Browser {
    fn go(&mut self, address: &str) {
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
struct BrowserDocument {
    owner: dioxus::signals::Owner,
    browser: Rc<RefCell<Browser>>,
}
impl document::Document for BrowserDocument {
    fn eval(&self, script: String) -> document::Eval {
        let routing = script.contains("window.addEventListener(\"popstate\"");
        document::Eval::new(self.owner.insert(Box::new(Page { browser: self.browser.clone(), routing }) as Box<dyn document::Evaluator>))
    }
}

fn last_push(browser: &Rc<RefCell<Browser>>) -> Option<String> {
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
