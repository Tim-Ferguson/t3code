//! The card for a secret an agent asked the user for. Port of `SecretRequestCard.tsx`.
//!
//! The typed value exists only in the form's own field and, for the moment of the click, in the
//! request: the field is uncontrolled (the component tracks whether it has text, not what it says),
//! it is read from the submitted form, and it is emptied as the answer is sent, so no state, row,
//! notice or error ever holds it. A failure says what the server said about the request, or a
//! generic line, never the value.
use crate::{
    runtime::{self, UiModel},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::Value;
use t3_client::secret_request as secret;
use t3_contracts::AuthEnvironmentScope;

/// The card, or the line an answered request leaves behind.
#[component]
pub(crate) fn SecretRequestCard(item: Value, visibility: String, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let shown = secret::display(&item, &visibility);
    let label = item["label"].as_str().unwrap_or("Secret requested").to_owned();
    match shown {
        secret::Display::Pending => rsx! {PendingForm {item,state,transport}},
        _ => {
            let mark = match shown {
                secret::Display::Answered(secret::Outcome::Saved) => "\u{2713}",
                secret::Display::Answered(_) => "\u{2212}",
                _ => "\u{1f512}",
            };
            rsx! {div {class:"secret-answered","data-item-type":"secret_request",
                span {class:"work-mark","aria-hidden":"true","{mark}"}
                span {"{label} \u{b7} {shown.label()}"}
            }}
        }
    }
}

#[component]
fn PendingForm(item: Value, state: Store<UiModel>, transport: runtime::TransportHandle) -> Element {
    let mut has_text = use_signal(|| false);
    let mut submitting = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let operable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationOperate));
    let field = format!("secret-{}", item["id"].as_str().unwrap_or_default());
    let label = item["label"].as_str().unwrap_or("Secret requested").to_owned();
    let reason = item["reason"].as_str().map(str::trim).filter(|reason| !reason.is_empty()).map(str::to_owned);
    let placeholder = item["placeholder"].as_str().unwrap_or(secret::DEFAULT_PLACEHOLDER).to_owned();
    let error_id = format!("{field}-error");
    let privacy_id = format!("{field}-privacy");

    // One answer per click, enabled again by the server's answer. `value` is the typed secret (a
    // save) or nothing (a decline); it is dropped when the request has been built.
    let send = {
        let (item, transport, field) = (item.clone(), transport.clone(), field.clone());
        use_callback(move |value: Option<String>| {
            if *submitting.peek() || !*operable.peek() {
                return;
            }
            let Some(payload) = secret::answer_input(&item, value.as_deref()) else { return };
            drop(value);
            submitting.set(true);
            error.set(None);
            // The field is emptied as the answer goes, so a secret is never left in the page.
            let clear = format!("var e=document.getElementById({});if(e){{e.value='';}}", serde_json::to_string(&field).expect("field id"));
            dioxus::document::eval(&clear);
            has_text.set(false);
            let transport = transport.clone();
            spawn(async move {
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, "secrets.answerRequest", payload).await;
                // A late answer for another connection is not this card's.
                if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                    return;
                }
                if let Err(message) = answer {
                    error.set(Some(secret::failure_message(&message)));
                }
                // The card turns into its answered line when the item updates.
                submitting.set(false);
            });
        })
    };
    rsx! {
        form {class:"secret-request","data-item-type":"secret_request","aria-label":"Secret request",autocomplete:"off",
            onsubmit:move |event| {
                event.prevent_default();
                let typed = event.values().into_iter().find_map(|(name, value)| match value {
                    dioxus::html::FormValue::Text(text) if name == "secret" => Some(text),
                    _ => None,
                });
                send.call(typed);
            },
            div {class:"secret-heading",
                label {r#for:"{field}","{label}"}
                if let Some(reason) = reason {p {class:"muted","{reason}"}}
            }
            div {class:"secret-field",
                // A masked field that is not a login: password managers must not offer to save or fill it.
                input {id:"{field}",name:"secret",r#type:"password",autocomplete:"off",autocapitalize:"off",spellcheck:"false","data-1p-ignore":"true","data-lpignore":"true","data-bwignore":"true",
                    placeholder:"{placeholder}","aria-label":"{label}",disabled:submitting() || !operable(),"aria-invalid":error.read().is_some().then_some("true"),
                    "aria-describedby":if error.read().is_some() {format!("{privacy_id} {error_id}")} else {privacy_id.clone()},
                    oninput:move |event| {
                        let filled = !event.value().trim().is_empty();
                        if filled != *has_text.peek() {has_text.set(filled);}
                    }}
                button {r#type:"submit",class:"primary","aria-label":"Save securely",title:if operable() {""} else {"This connection cannot answer requests."},disabled:submitting() || !has_text() || !operable(),"Save securely"}
            }
            if let Some(message) = error.read().as_ref() {p {id:"{error_id}",role:"alert",class:"failure","{message}"}}
            div {class:"secret-footer",
                p {id:"{privacy_id}",class:"muted","\u{1f6e1} {secret::PRIVACY_NOTE}"}
                button {r#type:"button","aria-label":"Decline secret request",disabled:submitting() || !operable(),onclick:move |_| send.call(None),"Decline"}
            }
        }
    }
}
