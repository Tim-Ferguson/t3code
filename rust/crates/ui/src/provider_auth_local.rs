//! Signing in to ChatGPT on this computer for an environment that is not on it (`CodexSetupSection.tsx`, the client-callback
//! mode): the sign-in page redirects to a loopback address that only this computer can reach, so the desktop app listens there
//! (`t3_desktop::provider_auth_callback`) and hands the redirect to the environment, instead of the person pasting it. The web
//! build has no listener and keeps the paste form.
use dioxus::prelude::*;

/// Whether this app can receive the sign-in's redirect for the environment it is connected to: the desktop app can, when the
/// environment is not on this computer (one that is, redirects to this computer's own port already).
pub(crate) fn applies(transport: &crate::runtime::TransportHandle) -> bool {
    #[cfg(feature = "desktop")]
    {
        !crate::runtime::connected_to_loopback(transport)
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = transport;
        false
    }
}

/// "Sign in on this computer": opens the sign-in page and waits for its redirect. `on_callback` gets the redirect's address.
#[component]
pub(crate) fn LocalSignIn(url: String, disabled: bool, on_callback: EventHandler<String>, on_error: EventHandler<String>) -> Element {
    #[cfg(feature = "desktop")]
    {
        // Only a sign-in this app would open is offered here; anything else keeps the paste form alone.
        if t3_client::chatgpt::authorization_request(&url).is_err() {
            return rsx! {};
        }
        let mut waiting = use_signal(|| false);
        let stop_url = url.clone();
        // Leaving the page ends the wait and frees the port.
        use_drop({
            let url = url.clone();
            move || t3_desktop::provider_auth_callback::cancel(&url)
        });
        return rsx! {
            if waiting() {
                button { r#type: "button", "aria-label": "Stop waiting for sign-in", onclick: move |_| t3_desktop::provider_auth_callback::cancel(&stop_url), "Stop waiting" }
            } else {
                button { r#type: "button", "aria-label": "Sign in on this computer", disabled,
                    onclick: move |_| {
                        let url = url.clone();
                        waiting.set(true);
                        spawn(async move {
                            let outcome = t3_desktop::provider_auth_callback::receive(
                                &url,
                                |page| async move { crate::desktop_shell::open_external(&page); true },
                                t3_desktop::provider_auth_callback::EXPIRY,
                            )
                            .await;
                            waiting.set(false);
                            match outcome {
                                Ok(callback) => on_callback.call(callback),
                                Err(cause) => on_error.call(cause),
                            }
                        });
                    },
                    "Sign in on this computer" }
            }
        };
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = (url, disabled, on_callback, on_error);
        rsx! {}
    }
}
