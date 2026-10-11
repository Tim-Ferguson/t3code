//! `/connect-agent`: where an outside agent's MCP sign-in lands after the server checks the request (the server's
//! `/oauth/mcp/authorize` redirects here with the agent's query). Port of `ConnectAgentSurface.tsx` and its route. The person picks
//! the most the agent may do, then approves with a pairing code, or in one click when this browser is already signed in to the
//! environment as an administrator, or denies. The server validates the request again on every call and answers a message to
//! show or the address the browser must follow back to the agent; the decisions are `t3_client::mcp_approval`.
//!
//! The page is a document of its own: the application, with its connection and its sidebar, is not started (the original's root
//! route renders only the outlet for this path).
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::mcp_approval as logic;
use t3_contracts::{AuthMcpApprovalDetails, AuthMcpClientAccess};

/// The path this page answers at.
pub(crate) const PATH: &str = "/connect-agent";

/// The query of the address when it is this page's, in the web build.
#[cfg(target_arch = "wasm32")]
pub(crate) fn requested() -> Option<(String, String)> {
    let location = web_sys::window()?.location();
    (location.pathname().ok()? == PATH).then(|| (location.origin().unwrap_or_default(), location.search().unwrap_or_default()))
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn requested() -> Option<(String, String)> {
    None
}

/// Where the browser is sent: back to the agent. Tests collect the addresses instead.
#[derive(Clone)]
pub(crate) struct Sink(pub std::rc::Rc<std::cell::RefCell<Vec<String>>>);

fn follow(url: &str, sink: &Option<Sink>) {
    if let Some(sink) = sink {
        sink.0.borrow_mut().push(url.to_owned());
        return;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let url = serde_json::to_string(url).unwrap_or_default();
        let _ = document::eval(&format!("window.location.replace({url})"));
    }
}

/// One call of the page: the status and the JSON the server answered, or none when it could not be reached.
async fn call(origin: &str, path: &str, body: &Value, cookie: Option<&str>) -> Option<(u16, Value)> {
    let mut request = reqwest::Client::new().post(format!("{}{path}", origin.trim_end_matches('/'))).json(body);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let response = request.send().await.ok()?;
    let status = response.status().as_u16();
    Some((status, response.json::<Value>().await.unwrap_or(Value::Null)))
}

#[derive(Clone, PartialEq)]
enum Loaded {
    Loading,
    Invalid(String),
    /// The browser is on its way back to the agent.
    Leaving,
    Ready(AuthMcpApprovalDetails),
}

/// The whole page: the stylesheet, the theme, and the surface.
#[component]
pub(crate) fn Page(origin: String, query: String) -> Element {
    let state = use_store(UiModel::default);
    crate::themes::use_themes(state);
    let theme = if *state.dark().read() { "dark" } else { "light" };
    rsx! {
        style { "{crate::STYLES}" }
        style { {include_str!("../assets/theme-tokens.css")} }
        div { class: "app connect-agent-page", "data-theme": theme,
            Surface { origin, query, cookie: None::<String> }
        }
    }
}

#[component]
pub(crate) fn Surface(origin: String, query: String, cookie: Option<String>) -> Element {
    let sink = try_consume_context::<Sink>();
    let authorization = use_hook({
        let query = query.clone();
        move || logic::authorization_from_query(&query)
    });
    let mut loaded = use_signal(|| Loaded::Loading);
    let mut access = use_signal(|| AuthMcpClientAccess::ReadOnly);
    let mut code = use_signal(String::new);
    let mut failure = use_signal(String::new);
    let mut pending = use_signal(|| None::<&'static str>);
    use_future({
        let (origin, authorization, cookie, sink) = (origin.clone(), authorization.clone(), cookie.clone(), sink.clone());
        move || {
            let (origin, authorization, cookie, sink) = (origin.clone(), authorization.clone(), cookie.clone(), sink.clone());
            async move {
                let answer = match call(&origin, "/oauth/mcp/approval", &authorization, cookie.as_deref()).await {
                    Some((status, body)) => logic::read_details(status, &body),
                    None => logic::Answer::Error(logic::UNREACHABLE.into()),
                };
                match answer {
                    logic::Answer::Redirect(url) => {
                        follow(&url, &sink);
                        loaded.set(Loaded::Leaving);
                    }
                    logic::Answer::Error(message) => loaded.set(Loaded::Invalid(message)),
                    logic::Answer::Ok(details) => loaded.set(Loaded::Ready(details)),
                }
            }
        }
    });
    let decide = {
        let (origin, authorization, cookie, sink) = (origin.clone(), authorization.clone(), cookie.clone(), sink.clone());
        use_callback(move |choice: &'static str| {
            let Loaded::Ready(details) = loaded.peek().clone() else { return };
            if pending.peek().is_some() {
                return;
            }
            pending.set(Some(choice));
            failure.set(String::new());
            let decision = if choice == "deny" { logic::deny_decision() } else { logic::approve_decision(&details, *access.peek(), &code.peek()) };
            let body = serde_json::json!({"authorization": authorization, "decision": decision});
            let (origin, cookie, sink) = (origin.clone(), cookie.clone(), sink.clone());
            spawn(async move {
                let answer = match call(&origin, "/oauth/mcp/decision", &body, cookie.as_deref()).await {
                    Some((status, body)) => logic::read_decision(status, &body),
                    None => logic::Answer::Error(logic::UNREACHABLE.into()),
                };
                match answer {
                    logic::Answer::Redirect(url) => {
                        follow(&url, &sink);
                        loaded.set(Loaded::Leaving);
                    }
                    other => {
                        pending.set(None);
                        failure.set(match other {
                            logic::Answer::Error(message) => message,
                            _ => logic::CANNOT_CONTINUE.into(),
                        });
                    }
                }
            });
        })
    };
    let heading = |title: &str, description: Element| {
        rsx! {
            p { class: "connect-agent-kicker", "Agent sign-in" }
            h1 { "{title}" }
            p { class: "muted", {description} }
        }
    };
    match loaded() {
        Loaded::Loading => rsx! {
            main { class: "connect-agent", "aria-busy": "true",
                {heading("Checking the sign-in request", rsx! { "One moment while this environment verifies the agent's request." })}
            }
        },
        Loaded::Leaving => rsx! {
            main { class: "connect-agent", "aria-busy": "true",
                {heading("Returning to your agent", rsx! { "This page can be closed once your agent has the answer." })}
            }
        },
        Loaded::Invalid(message) => rsx! {
            main { class: "connect-agent",
                {heading("This sign-in cannot continue", rsx! { "{message}" })}
                p { class: "muted", "Close this page and start the sign-in again from your agent." }
            }
        },
        Loaded::Ready(details) => {
            let one_click = logic::one_click_approves(&details, access());
            let can_approve = logic::can_approve(pending().is_some(), one_click, &code());
            let local = logic::redirects_to_this_computer(&details.redirect_host);
            let title = format!("Connect {}", details.client_name);
            rsx! {
                main { class: "connect-agent",
                    {heading(&title, rsx! { "This agent wants to use the threads in every project on " strong { "{details.environment_host}" } "." })}
                    p { class: "muted connect-agent-note",
                        if local { "The name is chosen by the agent. Approval returns to {details.redirect_host} on the computer that opened this page. Only approve a sign-in you just started." }
                        else { "The name is chosen by the agent. Approval gives access to whoever runs " strong { "{details.redirect_host}" } ". Only approve a sign-in you just started there." }
                    }
                    form { onsubmit: move |event| { event.prevent_default(); if can_approve { decide.call("approve") } },
                        fieldset { class: "connect-agent-access", role: "radiogroup", "aria-label": "What it may do",
                            legend { "What it may do" }
                            for (option, label, description) in logic::OPTIONS {
                                label { key: "{label}", class: if option == access() { "agent-access selected" } else { "agent-access" },
                                    input { r#type: "radio", name: "connect-agent-access", "aria-label": "{label}", checked: option == access(), onclick: move |_| access.set(option) }
                                    span { strong { "{label}" } br {} span { class: "muted", "{description}" } }
                                }
                            }
                            p { class: "muted", "Beyond read only, it can start, message and stop threads, and none of them can run with more than the mode you pick." }
                        }
                        if !one_click {
                            label { r#for: "connect-agent-pairing-code", "Pairing code" }
                            input { id: "connect-agent-pairing-code", "aria-label": "Pairing code", autocapitalize: "none", autocomplete: "one-time-code", spellcheck: "false", disabled: pending().is_some(), placeholder: "Paste a one-time pairing code", value: "{code}", oninput: move |event| code.set(event.value()) }
                            p { class: "muted", "Create one in Settings \u{2192} Connections, or run " code { "t3 auth pairing create" } " on this machine." }
                        }
                        if !failure().is_empty() { p { role: "alert", class: "publish-error", "{failure}" } }
                        div { class: "connect-agent-actions",
                            button { class: "primary", r#type: "button", "aria-label": "Approve", disabled: !can_approve, onclick: move |_| decide.call("approve"), if pending() == Some("approve") { "Approving\u{2026}" } else { "Approve" } }
                            button { r#type: "button", "aria-label": "Deny", disabled: pending().is_some(), onclick: move |_| decide.call("deny"), if pending() == Some("deny") { "Denying\u{2026}" } else { "Deny" } }
                        }
                    }
                }
            }
        }
    }
}
