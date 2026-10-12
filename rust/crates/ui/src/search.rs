//! Finding a thread: the sidebar's search box and the palette's thread results share this hook.
//! Titles match as you type; messages are searched on the server after a pause, and only for
//! queries it accepts. Every connected environment is asked, each on its own connection
//! (`useThreadSearch(environmentIds, query)`), and a failed or slow one leaves the others' answers
//! and the title matches. An answer that is not for the latest query, connection and thread list is
//! never shown: the request is dropped when the query changes, and what is shown is checked
//! against the threads the sidebar has right now.
use crate::{
    fleet,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{
    palette::highlight,
    thread_search::{self, Candidate, Hit, Match},
};
use t3_contracts::{AuthEnvironmentScope, EnvironmentId};

/// The server's latest answer, and the query it answers.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Answer {
    pub query: String,
    pub matches: Vec<Match>,
    pub loading: bool,
}

#[cfg(test)]
thread_local! {
    /// Lets a test hold the reply to one query on its way to the screen.
    pub(crate) static HOLD: std::cell::RefCell<Option<Hold>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
thread_local! {
    /// Every query that was sent to the server, in order.
    pub(crate) static REQUESTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct Hold {
    pub query: String,
    /// Set once the reply for `query` is in hand.
    pub reached: std::rc::Rc<std::cell::Cell<bool>>,
    /// Set if the request was dropped while held, which is what a newer query does to it.
    pub dropped: std::rc::Rc<std::cell::Cell<bool>>,
    pub release: std::rc::Rc<std::cell::RefCell<Option<futures_channel::oneshot::Receiver<()>>>>,
}

/// The environments a search asks: the connected ones whose session may read threads.
fn searchable(state: Store<UiModel>) -> Vec<EnvironmentId> {
    let mut environments: Vec<EnvironmentId> = state.live().read().keys().cloned().collect();
    if let Some(open) = state.destination().read().clone().filter(|open| !environments.contains(open)) {
        environments.push(open);
    }
    environments.retain(|environment| fleet::allows(state, environment, AuthEnvironmentScope::OrchestrationRead));
    environments
}

/// Asks every environment's server about `query` (after the pause) while its connection can read threads.
pub(crate) fn use_search(state: Store<UiModel>, transport: TransportHandle, query: impl Fn() -> String + 'static) -> Signal<Answer> {
    let mut answer = use_signal(Answer::default);
    use_resource(move || {
        let typed = query();
        let environments = searchable(state);
        let transport = transport.clone();
        async move {
            let Some(asked) = thread_search::request_query(&typed).filter(|_| !environments.is_empty()) else {
                if *answer.peek() != Answer::default() {
                    answer.set(Answer::default());
                }
                return;
            };
            runtime::delay(thread_search::DEBOUNCE_MS).await;
            // Each question goes out on the connection of its environment and is answered for that connection alone.
            let links: Vec<(EnvironmentId, TransportHandle)> = environments.into_iter().filter_map(|environment| transport.link_of(&environment).map(|link| (environment, link))).collect();
            let owners: Vec<_> = links.iter().map(|(_, link)| runtime::link_owner(link)).collect();
            if owners.iter().all(Option::is_none) {
                return;
            }
            if answer.peek().query != asked {
                answer.write().loading = true;
            }
            #[cfg(test)]
            REQUESTS.with(|requests| requests.borrow_mut().push(asked.clone()));
            let results = futures_util::future::join_all(links.iter().map(|(_, link)| runtime::request_value_reported(link.clone(), state, "orchestration.searchThreads", json!({"query":asked})))).await;
            #[cfg(test)]
            hold(&asked).await;
            let mut matches = vec![];
            for (((environment, link), owner), result) in links.iter().zip(owners).zip(results) {
                // The reply belongs to a connection that has since changed.
                if owner.is_none() || runtime::link_owner(link) != owner {
                    continue;
                }
                // A failed search leaves title matches, as the original does.
                matches.extend(result.and_then(|value| thread_search::decode_matches(&value)).unwrap_or_default().into_iter().map(|found| Match { environment: environment.to_string(), ..found }));
            }
            answer.set(Answer { query: asked, matches, loading: false });
        }
    });
    answer
}
#[cfg(test)]
async fn hold(query: &str) {
    struct Dropped(std::rc::Rc<std::cell::Cell<bool>>, bool);
    impl Drop for Dropped {
        fn drop(&mut self) {
            if self.1 {
                self.0.set(true);
            }
        }
    }
    let held = HOLD.with(|slot| slot.borrow().clone()).filter(|hold| hold.query == query);
    if let Some(hold) = held {
        let mut guard = Dropped(hold.dropped.clone(), true);
        hold.reached.set(true);
        let receiver = hold.release.borrow_mut().take();
        if let Some(receiver) = receiver {
            let _ = receiver.await;
        }
        guard.1 = false;
    }
}

/// The threads a search covers, in the order the sidebar lists them: each environment's, by project.
pub(crate) fn candidates(state: Store<UiModel>) -> Vec<Candidate> {
    fleet::read(state, |shells| {
        shells
            .iter()
            .flat_map(|shell| {
                let environment = shell.environment.to_string();
                shell.snapshot.projects.iter().flat_map(move |project| {
                    let environment = environment.clone();
                    shell.snapshot.threads.iter().filter(move |thread| thread.project_id == project.id && thread.deleted_at.is_none() && thread.archived_at.is_none()).map(move |thread| Candidate { environment: environment.clone(), id: thread.id.to_string(), project_id: thread.project_id.to_string(), title: thread.title.clone() })
                })
            })
            .collect()
    })
}
/// The hits for what was typed, with the server's matches only while they answer it.
pub(crate) fn hits(state: Store<UiModel>, typed: &str, answer: &Answer) -> Vec<Hit> {
    let current = thread_search::request_query(typed).filter(|query| *query == answer.query);
    let matches = if current.is_some() { answer.matches.as_slice() } else { &[] };
    thread_search::combine(&candidates(state), typed, matches)
}
/// Whether the server has not yet answered what was typed.
pub(crate) fn pending(typed: &str, answer: &Answer) -> bool {
    thread_search::request_query(typed).is_some_and(|query| answer.loading || query != answer.query)
}
/// The project's title, and the environment's name after it while more than one is connected.
pub(crate) fn project_title(state: Store<UiModel>, environment: &str, project_id: &str) -> String {
    let Ok(environment) = EnvironmentId::new(environment) else { return String::new() };
    let title = fleet::read(state, |shells| shells.iter().filter(|shell| *shell.environment == environment).find_map(|shell| shell.snapshot.projects.iter().find(|project| project.id.as_str() == project_id).map(|project| project.title.to_string())).unwrap_or_default());
    if fleet::count(state) > 1 { format!("{title} \u{b7} {}", fleet::label(state, &environment)) } else { title }
}

/// A message excerpt with the matched words marked.
#[component]
pub(crate) fn Excerpt(user: bool, snippet: String, query: String) -> Element {
    let parts = highlight(&snippet, &query);
    rsx! {
        span { class: "search-excerpt",
            span { class: if user { "excerpt-source user" } else { "excerpt-source assistant" }, if user { "You: " } else { "Agent: " } }
            for (index, (text, marked)) in parts.into_iter().enumerate() {
                if marked { mark { key: "{index}", "{text}" } } else { "{text}" }
            }
        }
    }
}

/// The sidebar's search box. While something is typed, its results take the place of the project
/// list (the list follows this element and is hidden by `data-searching`).
#[component]
pub(crate) fn SidebarSearch(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let mut query = use_signal(String::new);
    let mut active = use_signal(|| 0usize);
    let answer = use_search(state, transport.clone(), move || query());
    let typed = query.read().clone();
    let searching = !typed.trim().is_empty();
    let found = if searching { hits(state, &typed, &answer.read()) } else { vec![] };
    let waiting = searching && pending(&typed, &answer.read());
    let count = found.len();
    let shown = (*active.read()).min(count.saturating_sub(1));
    let open = {
        let (transport, mut query) = (transport.clone(), query);
        move |(environment, id): (String, String)| {
            query.set(String::new());
            if let Ok(environment) = EnvironmentId::new(&environment) {
                runtime::select_thread_in(&transport, state, &environment, id);
            }
        }
    };
    let ids: Vec<(String, String)> = found.iter().map(|hit| (hit.thread.environment.clone(), hit.thread.id.clone())).collect();
    rsx! {
        div { class: "sidebar-search", "data-searching": "{searching}",
            div { class: "search-field",
                input { r#type: "search", role: "combobox", "aria-label": "Search threads", "aria-autocomplete": "list", "aria-expanded": "{count > 0}", "aria-controls": if count > 0 { "thread-search-results" }, placeholder: "Search", value: "{typed}",
                    oninput: move |event| { query.set(event.value()); active.set(0); },
                    onkeydown: {
                        let (ids, mut open) = (ids.clone(), open.clone());
                        move |event| {
                            // Committing an IME candidate uses these keys; it must not move or choose.
                            if event.is_composing() { return; }
                            match event.key() {
                                Key::Escape if searching => { event.prevent_default(); event.stop_propagation(); query.set(String::new()); active.set(0); }
                                Key::ArrowDown if count > 0 => { event.prevent_default(); active.set(thread_search::step(shown, count, true)); }
                                Key::ArrowUp if count > 0 => { event.prevent_default(); active.set(thread_search::step(shown, count, false)); }
                                Key::Enter => { event.prevent_default(); if let Some(id) = ids.get(shown) { open(id.clone()); } }
                                _ => {}
                            }
                        }
                    } }
                if searching { button { class: "search-clear", r#type: "button", "aria-label": "Clear thread search", onclick: move |_| { query.set(String::new()); active.set(0); }, "\u{d7}" } }
            }
            if searching {
                if count > 0 {
                    ul { id: "thread-search-results", class: "search-results", role: "listbox", "aria-label": "Thread search results",
                        for (index, hit) in found.into_iter().enumerate() {
                            li { key: "{hit.thread.environment}:{hit.thread.id}", role: "presentation",
                                button { class: if index == shown { "search-result active" } else { "search-result" }, r#type: "button", role: "option", tabindex: "-1", "aria-label": "{hit.thread.title}", "aria-selected": "{index == shown}",
                                    onmousemove: move |_| { if *active.peek() != index { active.set(index); } },
                                    onclick: { let (id, mut open) = ((hit.thread.environment.clone(), hit.thread.id.clone()), open.clone()); move |_| open(id.clone()) },
                                    span { class: "thread-title", "{hit.thread.title}" }
                                    span { class: "search-project", "{project_title(state, &hit.thread.environment, &hit.thread.project_id)}" }
                                    if let Some(found) = hit.found { Excerpt { user: found.user, snippet: found.snippet, query: typed.clone() } }
                                }
                            }
                        }
                    }
                } else {
                    p { class: "search-status", role: "status", if waiting { "{thread_search::STATUS_SEARCHING}" } else { "{thread_search::STATUS_NONE}" } }
                }
            }
        }
    }
}
