//! The desktop app's own switches on the Connections page: the local environment (whether this app starts a server on this computer),
//! network access (whether that server listens beyond this machine), and Tailscale HTTPS, with the addresses a device can reach the
//! machine at. Port of `LocalEnvironmentSetting.tsx` and the network access, endpoint and Tailscale rows of `ConnectionsSettings.tsx`.
//! What is saved and restarted is the shell's (`Control`, provided by `desktop_shell`; a browser provides none and the section draws
//! nothing); this file asks, confirms and shows. A change that needs the server to start again says so before it is made.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel},
    settings_common::{Confirm, connected_allows},
};
use dioxus::prelude::*;
use futures_util::future::LocalBoxFuture;
use std::rc::Rc;
use t3_client::access::{self as logic, Endpoint};
use t3_contracts::AuthEnvironmentScope;

/// `DesktopServerExposureState`, as the section needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExposureView {
    pub network_accessible: bool,
    pub endpoint_url: Option<String>,
    pub advertised_host: Option<String>,
    pub tailscale_serve_enabled: bool,
    pub tailscale_serve_port: u16,
}

/// What the shell does for the section.
pub(crate) trait Control {
    fn local_environment_enabled(&self) -> bool;
    /// Saves the choice and starts the app again; the app ends here when it works.
    fn set_local_environment_enabled(&self, enabled: bool) -> Result<(), String>;
    /// None while no server of this app's own runs (the local environment is off, or the server is the person's).
    fn exposure(&self) -> Option<ExposureView>;
    fn set_network_accessible(&self, accessible: bool) -> Result<ExposureView, String>;
    fn set_tailscale_serve(&self, enabled: bool, port: u16) -> Result<ExposureView, String>;
    fn endpoints(&self) -> LocalBoxFuture<'static, Vec<Endpoint>>;
}

#[derive(Clone)]
pub(crate) struct Handle(pub Rc<dyn Control>);

pub(crate) const DEFAULT_SERVE_PORT: u16 = 443;

fn is_tailscale_https(endpoint: &Endpoint) -> bool {
    endpoint.id.starts_with("tailscale-magicdns:")
}

/// `parseInt` of the port field, as the original accepts it: digits only, from 1 to 65535.
pub(crate) fn parse_port(text: &str) -> Option<u16> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|port| *port >= 1)
}

/// The HTTPS address Tailscale would serve on `port` (`pendingTailscaleServeBaseUrl`): the MagicDNS one, with the port when it is not 443.
pub(crate) fn serve_url(base: &str, port: Option<u16>) -> String {
    let Some(port) = port.filter(|port| *port != DEFAULT_SERVE_PORT) else { return base.to_owned() };
    match url::Url::parse(base) {
        Ok(mut url) => {
            let _ = url.set_port(Some(port));
            url.to_string()
        }
        Err(_) => base.to_owned(),
    }
}

/// The addresses of the open network: none while it is closed, and never the Tailscale HTTPS one, which has its own row.
fn network_endpoints(view: Option<&ExposureView>, endpoints: &[Endpoint]) -> Vec<Endpoint> {
    if view.is_some_and(|view| view.network_accessible) { endpoints.iter().filter(|endpoint| !is_tailscale_https(endpoint)).cloned().collect() } else { Vec::new() }
}

/// What the network access row says under its title (`renderNetworkAccessRow`), when the endpoint to name is not among the list.
fn fallback_description(view: &ExposureView) -> String {
    match (&view.endpoint_url, &view.advertised_host) {
        (Some(url), _) => format!("Reachable at {url}"),
        (None, Some(host)) => format!("Exposed on all interfaces. Pairing links use {host}."),
        (None, None) => "Exposed on all interfaces.".to_owned(),
    }
}

fn restart_warning(accessible: bool) -> &'static str {
    if accessible {
        "Let your other devices connect to T3 Code over the network. Pair devices to give them access. T3 Code will restart."
    } else {
        "Devices connected over your local network will disconnect. Existing tunnels, such as T3 Connect or Tailscale HTTPS, keep working. T3 Code will restart."
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    None,
    Network(bool),
    LocalEnvironment(bool),
    TailscaleSetup,
    TailscaleDisable,
}

/// Mounted on the Connections page of every build; draws nothing where the shell offers no control.
#[component]
pub(crate) fn Section(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let Some(control) = try_consume_context::<Handle>() else { return rsx! {} };
    // The network is the server of this app's own to open, and only by someone who may maintain it.
    let can_manage = connected_allows(state, AuthEnvironmentScope::EnvironmentMaintain) && runtime::connected_to_loopback(&transport);
    rsx! { Rows { state, can_manage, control } }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[component]
pub(crate) fn Rows(state: Store<UiModel>, can_manage: bool, control: Handle) -> Element {
    let control = control.0;
    let mut view = use_signal({
        let control = control.clone();
        move || control.exposure()
    });
    let mut endpoints = use_signal(Vec::<Endpoint>::new);
    let mut default_key = use_signal(|| None::<String>);
    let mut expanded = use_signal(|| false);
    let mut dialog = use_signal(|| Dialog::None);
    let mut busy = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let mut port_text = use_signal(|| DEFAULT_SERVE_PORT.to_string());

    // The addresses are read when the section opens and again after each change: Tailscale answers slowly, and a change moves them.
    let reload = use_callback({
        let control = control.clone();
        move |(): ()| {
            let found = control.endpoints();
            spawn(async move { endpoints.set(found.await) });
        }
    });
    use_hook(|| {
        reload.call(());
        spawn(async move { default_key.set(crate::access_page::read_default_key().await) });
    });

    let managed = can_manage && view.read().is_some();
    let current = view.read().clone();
    let all = endpoints.read().clone();
    let visible = network_endpoints(current.as_ref(), &all);
    let default_endpoint = logic::select_pairing_endpoint(&visible, default_key.read().as_deref()).cloned();
    let tailscale = all.iter().find(|endpoint| is_tailscale_https(endpoint)).cloned();
    let enabled = control.local_environment_enabled();
    let working = *busy.read();

    // One change at a time; a refusal is said where it happened and in a notice.
    let apply = use_callback({
        let control = control.clone();
        move |dialog_open: Dialog| {
            if *busy.peek() {
                return;
            }
            busy.set(true);
            failure.set(None);
            let outcome: Result<Option<ExposureView>, (&str, String)> = match dialog_open {
                Dialog::Network(accessible) => control.set_network_accessible(accessible).map(Some).map_err(|message| ("Could not update network access", message)),
                Dialog::TailscaleSetup => match parse_port(&port_text.peek()) {
                    Some(port) => control.set_tailscale_serve(true, port).map(Some).map_err(|message| ("Could not configure Tailscale HTTPS", message)),
                    None => {
                        busy.set(false);
                        return;
                    }
                },
                Dialog::TailscaleDisable => {
                    let port = view.peek().as_ref().map_or(DEFAULT_SERVE_PORT, |view| view.tailscale_serve_port);
                    control.set_tailscale_serve(false, port).map(Some).map_err(|message| ("Could not disable Tailscale HTTPS", message))
                }
                Dialog::LocalEnvironment(on) => control.set_local_environment_enabled(on).map(|()| None).map_err(|message| ("Could not change the local environment", message)),
                Dialog::None => Ok(None),
            };
            match outcome {
                Ok(Some(next)) => {
                    view.set(Some(next));
                    dialog.set(Dialog::None);
                    busy.set(false);
                    reload.call(());
                }
                // The app starts again and ends this window; if it is still here, nothing was changed.
                Ok(None) => busy.set(false),
                Err((title, message)) => {
                    dialog.set(Dialog::None);
                    notice::push(state, Level::Error, title, message.clone());
                    failure.set(Some(message));
                    busy.set(false);
                }
            }
        }
    });

    let switch = |label: &'static str, checked: bool, disabled: bool, on: EventHandler<bool>| -> Element {
        rsx! { input { r#type: "checkbox", role: "switch", "aria-label": "{label}", checked, disabled, onchange: move |event| on.call(event.checked()) } }
    };

    let network_switch = switch("Enable network access", current.as_ref().is_some_and(|view| view.network_accessible), working, EventHandler::new(move |on| dialog.set(Dialog::Network(on))));
    let local_switch = switch("Local environment", enabled, working, EventHandler::new(move |on| dialog.set(Dialog::LocalEnvironment(on))));
    let tailscale_switch = tailscale.as_ref().map(|endpoint| {
        let on = endpoint.available;
        let saved_port = current.as_ref().map_or(DEFAULT_SERVE_PORT, |view| view.tailscale_serve_port);
        switch("Enable Tailscale HTTPS", on, working, EventHandler::new(move |turn_on: bool| {
            if turn_on {
                port_text.set(saved_port.to_string());
                dialog.set(Dialog::TailscaleSetup);
            } else {
                dialog.set(Dialog::TailscaleDisable);
            }
        }))
    });

    let hidden = visible.len().saturating_sub(1);
    // TODO(original ConnectionsSettings.tsx renderTailscaleRow): the address is looked up only while the network or Serve is open
    // (`should_ask_tailscale`), so with both closed there is no row switch even when Tailscale runs; Serve cannot be set up first. Copied.
    let tailscale_description = match &tailscale {
        Some(endpoint) if endpoint.available => endpoint.http_base_url.clone(),
        Some(_) => "Use Tailscale Serve to expose this backend through a MagicDNS HTTPS URL.".to_owned(),
        None => "Start Tailscale to set up HTTPS access through MagicDNS.".to_owned(),
    };
    let shown_dialog = *dialog.read();
    let pending_url = tailscale.as_ref().map(|endpoint| serve_url(&endpoint.http_base_url, parse_port(&port_text.read())));
    let port_ok = parse_port(&port_text.read()).is_some();
    rsx! {
        section { class: "sc-section", id: "this-machine", "aria-label": "This machine",
            header { h2 { "This machine" } }
            div { class: "setting-row", "data-setting": "local-environment",
                div { class: "font-description", strong { "Local environment" }
                    p { class: "muted", if enabled { "Run agents on this computer. Turn off to use T3 Code only with remote environments." } else { "Turned off. Agents only run in remote environments." } } }
                div { class: "font-controls", {local_switch} }
            }
            if managed {
                div { class: "setting-row", "data-setting": "network-access",
                    div { class: "font-description", strong { "Network access" }
                        p { class: "muted",
                            if let Some(shown) = current.as_ref().filter(|view| view.network_accessible) {
                                match &default_endpoint {
                                    Some(endpoint) => rsx! {
                                        "Reachable at {endpoint.http_base_url}"
                                        if hidden > 0 {
                                            button { r#type: "button", "aria-expanded": "{expanded()}", "aria-label": "Show the other addresses",
                                                onclick: move |_| { let next = !expanded(); expanded.set(next); },
                                                if expanded() { " Hide" } else { " +{hidden}" } }
                                        }
                                    },
                                    None => rsx! { "{fallback_description(shown)}" },
                                }
                            } else { "Limited to this machine." }
                        }
                        if let Some(message) = failure() { p { class: "publish-error", role: "alert", crate::error_text::ErrorText { text: message } } }
                    }
                    div { class: "font-controls", {network_switch} }
                }
                if expanded() {
                    for endpoint in visible.iter() {
                        {
                            let key = endpoint.preference_key();
                            let is_default = default_endpoint.as_ref().is_some_and(|chosen| chosen.id == endpoint.id);
                            let pick = key.clone();
                            rsx! {
                                article { key: "{endpoint.id}", class: "access-row", "aria-label": "Address {endpoint.label}",
                                    div { class: "access-main",
                                        strong { "{endpoint.label}" if !endpoint.available { span { class: "access-badge", " Setup required" } } if is_default { span { class: "access-badge", " Default" } } }
                                        p { class: "muted", "{endpoint.http_base_url}" }
                                    }
                                    if !is_default && endpoint.available {
                                        button { r#type: "button", "aria-label": "Set {endpoint.label} as default",
                                            onclick: move |_| { default_key.set(Some(pick.clone())); crate::access_page::write_default_key(&pick); },
                                            "Set as default" }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "setting-row", "data-setting": "tailscale-https",
                    div { class: "font-description", strong { "Tailscale HTTPS" } p { class: "muted", "{tailscale_description}" } }
                    div { class: "font-controls", if let Some(switch) = tailscale_switch { {switch} } }
                }
            }
        }
        match shown_dialog {
            Dialog::None => rsx! {},
            Dialog::Network(accessible) => rsx! {
                Confirm { label: "network access", title: if accessible { "Enable network access?" } else { "Disable network access?" }, body: restart_warning(accessible),
                    confirm: if working { "Restarting\u{2026}" } else if accessible { "Restart and enable" } else { "Restart and disable" }, busy: working,
                    oncancel: move |()| { if !*busy.peek() { dialog.set(Dialog::None); } }, onconfirm: move |()| apply.call(Dialog::Network(accessible)) }
            },
            Dialog::LocalEnvironment(on) => rsx! {
                Confirm { label: "local environment", title: if on { "Turn on local environment?" } else { "Turn off local environment?" },
                    body: if on { "T3 Code will restart and start running a server on this computer again." } else { "T3 Code will restart without running a server on this computer. Any agents and terminals running here will stop, and other devices will no longer be able to connect to this computer. Your projects, history, and remote environments are unaffected." },
                    confirm: if working { "Restarting\u{2026}" } else if on { "Restart and turn on" } else { "Restart and turn off" }, busy: working,
                    oncancel: move |()| { if !*busy.peek() { dialog.set(Dialog::None); } }, onconfirm: move |()| apply.call(Dialog::LocalEnvironment(on)) }
            },
            Dialog::TailscaleDisable => rsx! {
                Confirm { label: "Tailscale HTTPS", title: "Disable Tailscale HTTPS?", body: "T3 Code will restart the local backend without Tailscale Serve.",
                    confirm: if working { "Restarting\u{2026}" } else { "Restart and disable" }, busy: working,
                    oncancel: move |()| { if !*busy.peek() { dialog.set(Dialog::None); } }, onconfirm: move |()| apply.call(Dialog::TailscaleDisable) }
            },
            Dialog::TailscaleSetup => rsx! {
                div { class: "modal-backdrop",
                    div { class: "dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "tailscale-heading", tabindex: "-1",
                        onkeydown: move |event| { if event.key() == Key::Escape && !*busy.peek() { event.stop_propagation(); dialog.set(Dialog::None); } },
                        h2 { id: "tailscale-heading", "Set up Tailscale HTTPS?" }
                        p { class: "muted", "T3 Code will restart the local backend with Tailscale Serve enabled and ask Tailscale to proxy HTTPS traffic to this backend." }
                        label { r#for: "tailscale-port", "HTTPS port" }
                        input { id: "tailscale-port", r#type: "number", "aria-label": "HTTPS port", min: "1", max: "65535", step: "1", disabled: working, value: "{port_text}", oninput: move |event| port_text.set(event.value()) }
                        if !port_ok { p { class: "publish-error", role: "alert", "Enter a port from 1 to 65535." } }
                        p { class: "muted", "HTTPS endpoint" }
                        p { "aria-label": "HTTPS endpoint", "{pending_url.clone().unwrap_or_else(|| \"Pending MagicDNS endpoint\".to_owned())}" }
                        div { class: "dialog-actions",
                            button { r#type: "button", "aria-label": "Cancel Tailscale HTTPS", disabled: working, onclick: move |_| dialog.set(Dialog::None), "Cancel" }
                            button { class: "primary", r#type: "button", "aria-label": "Enable", disabled: working || !port_ok, onclick: move |_| apply.call(Dialog::TailscaleSetup), if working { "Restarting\u{2026}" } else { "Enable" } }
                        }
                    }
                }
            },
        }
    }
}

#[cfg(test)]
#[path = "desktop_network_tests.rs"]
mod tests;
