//! Settings, Source control: which version control tools and hosting providers the environment's
//! server can use, whether each is signed in and as whom, how to fix one that is not, and the
//! credentials the server keeps for GitHub and Bitbucket. Port of `SourceControlSettings.tsx` and
//! its GitHub and Bitbucket panels, for the one environment this app is connected to.
//!
//! A scan runs when the page opens, when asked for, and after a credential changes; nothing
//! polls. The result is a signal of this page, so a scan repaints the page and nothing else. After
//! a lost connection the last result stays, marked as out of date, and nothing is saved. Saved
//! tokens never come back from the server, so none is ever shown: only whether one is set.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt, View},
    settings_common::{Tabs, connected_allows},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{
    source_control::{self as logic, BitbucketDraft, Credential, Dot, HostChange, Item, Part},
};
use t3_contracts::{AuthEnvironmentScope, SourceControlDiscoveryResult, SourceControlProviderDiscoveryItem, SourceControlProviderKind, VcsDiscoveryItem};

pub(crate) const READ_ONLY: &str = "This connection can view source control tools but cannot change their settings.";

/// What the last scans told the page. `stale` is set when the connection is lost after a result
/// arrived: the result stays for reading and is marked as old.
#[derive(Debug, Clone, PartialEq, Default)]
struct Scan {
    result: Option<SourceControlDiscoveryResult>,
    pending: bool,
    error: Option<String>,
    stale: bool,
}

#[component]
pub(crate) fn Page(state: Store<UiModel>, transport: TransportHandle) -> Element {
    let destination = state.destination().read().clone();
    let label = destination.as_ref().and_then(|id| state.environments().read().records.get(id).map(|record| record.label.clone())).unwrap_or_default();
    rsx! { section { class: "settings-page source-control-page",
        Tabs { state, current: View::SourceControl }
        h1 { "Source control" }
        if let Some(destination) = destination {
            Environment { key: "{destination}", state, transport, environment_label: label }
        } else {
            p { "Connect to an environment to inspect its version control tools and hosting integrations." }
            button { onclick: move |_| state.view().set(View::Connections), "Open connections" }
        }
    } }
}

#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Vcs(VcsDiscoveryItem),
    Provider(SourceControlProviderDiscoveryItem),
}
impl Entry {
    fn item(&self) -> Item<'_> {
        match self {
            Self::Vcs(item) => Item::Vcs(item),
            Self::Provider(item) => Item::Provider(item),
        }
    }
}

#[component]
fn Environment(state: Store<UiModel>, transport: TransportHandle, environment_label: String) -> Element {
    let mut scan = use_signal(Scan::default);
    let mut rescans = use_signal(|| 0u64);
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    // Counts saves the server accepted; a form clears what was typed into it when this moves,
    // not when the save is sent, so a refused save keeps the user's input.
    let mut saved = use_signal(|| 0u64);
    let readable = use_memo(move || connected_allows(state, AuthEnvironmentScope::OrchestrationRead));
    let settable = use_memo(move || connected_allows(state, AuthEnvironmentScope::SettingsWrite) && !scan.read().stale);

    // The scan: once when the page opens with a connection that can read, again whenever asked
    // or when a connection returns. A newer scan drops the one in flight, and a lost connection
    // only marks the result old.
    {
        let transport = transport.clone();
        use_resource(move || {
            let (transport, readable, _) = (transport.clone(), readable(), rescans());
            async move {
                if !readable {
                    if scan.peek().result.is_some() && !scan.peek().stale {
                        scan.write().stale = true;
                    }
                    return;
                }
                let Some(owner) = runtime::connection_owner(&transport, state) else { return };
                if !scan.peek().pending {
                    scan.write().pending = true;
                }
                let result = runtime::request_value_reported(transport.clone(), state, logic::DISCOVER, json!({})).await;
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                match result.and_then(|value| logic::decode_discovery(&value)) {
                    Ok(result) => *scan.write() = Scan { result: Some(result), pending: false, error: None, stale: false },
                    Err(error) => {
                        let mut scan = scan.write();
                        scan.pending = false;
                        scan.error = Some(error);
                    }
                }
            }
        });
    }

    // Saving a setting: the server's settings change, the configuration the rest of the app
    // reads follows (by its own stream, else one request), and the scan runs again because a
    // credential changes what the hosts say.
    let save = {
        let transport = transport.clone();
        use_callback(move |patch: Value| {
            if *saving.peek() || !*settable.peek() {
                return;
            }
            let Some(owner) = runtime::connection_owner(&transport, state) else { return };
            saving.set(true);
            failure.set(None);
            let transport = transport.clone();
            spawn(async move {
                let mut result = runtime::request_value_reported(transport.clone(), state, "server.updateSettings", patch).await.map(|_| ());
                if result.is_ok() && runtime::connection_owner(&transport, state).as_ref() == Some(&owner) && !runtime::live_config_active(&transport, state) {
                    result = runtime::request_value_reported(transport.clone(), state, "server.getConfig", json!({})).await.map(|_| ());
                }
                saving.set(false);
                if runtime::connection_owner(&transport, state).as_ref() != Some(&owner) {
                    return;
                }
                match result {
                    Ok(()) => {
                        saved += 1;
                        rescans += 1;
                    }
                    Err(message) => failure.set(Some(format!("Could not save source control settings. {message}"))),
                }
            });
        })
    };

    let snapshot = scan.read().clone();
    let mut revealed = use_signal(|| false);
    let (vcs, providers): (Vec<Entry>, Vec<Entry>) = match snapshot.result.as_ref() {
        Some(result) => {
            let (vcs, providers) = logic::rows(result);
            let own = |item: Item<'_>| match item {
                Item::Vcs(item) => Entry::Vcs(item.clone()),
                Item::Provider(item) => Entry::Provider(item.clone()),
            };
            (vcs.into_iter().map(own).collect(), providers.into_iter().map(own).collect())
        }
        None => (vec![], vec![]),
    };
    let scanning = snapshot.pending;
    let rescan = rsx! { button { class: "rescan", "aria-label": "Rescan server environment", title: "Rescan Git and hosting integrations", disabled: scanning || !readable() || snapshot.stale, onclick: move |_| rescans += 1, "Rescan" } };
    rsx! {
        p { class: "muted", "Tools and hosting integrations on the server of {environment_label}." }
        if !connected_allows(state, AuthEnvironmentScope::SettingsWrite) { p { class: "muted", role: "status", "{READ_ONLY}" } }
        if snapshot.stale { p { class: "stale-banner", role: "status", "This scan may be out of date. It is shown as last received; reconnect to scan again." } }
        if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
        if *saving.read() { p { role: "status", "Saving source control settings\u{2026}" } }
        FetchInterval { state, saving: *saving.read(), writable: settable(), onsave: save }
        if snapshot.result.is_none() {
            section { class: "sc-section",
                h2 { "Server environment" }
                if scanning { p { class: "muted", role: "status", "Scanning the server environment\u{2026}" } }
                else if let Some(error) = snapshot.error.clone() { p { class: "error-banner", role: "alert", "Could not scan the server environment. {error}" } }
                else if !readable() { p { class: "muted", role: "status", "Connect to see this environment's version control tools and hosting integrations." } }
                else { p { class: "muted", role: "status", "Nothing detected yet. Install Git on the server, add optional hosting integrations or credentials your workspace needs, then rescan." } }
                {rescan}
            }
        } else {
            if let Some(error) = snapshot.error.clone() { p { class: "error-banner", role: "alert", "Could not scan the server environment. {error}" } }
            if scanning { p { class: "muted", role: "status", "Scanning the server environment\u{2026}" } }
            if !vcs.is_empty() {
                section { class: "sc-section", "aria-label": "Version control",
                    header { h2 { "Version Control" } {rescan.clone()} }
                    for entry in vcs.iter() { DiscoveryRow { key: "{entry.item().key()}", entry: entry.clone(), revealed: *revealed.read(), details: false, onreveal: move |_| revealed.toggle() } }
                }
            }
            if !providers.is_empty() {
                section { class: "sc-section", "aria-label": "Source control providers",
                    header { h2 { "Source Control Providers" } if vcs.is_empty() { {rescan.clone()} } }
                    for entry in providers.iter() {
                        DiscoveryRow { key: "{entry.item().key()}", entry: entry.clone(), revealed: *revealed.read(), details: matches!(entry, Entry::Provider(provider) if matches!(provider.kind, SourceControlProviderKind::Github | SourceControlProviderKind::Bitbucket)), onreveal: move |_| revealed.toggle(),
                            if let Entry::Provider(provider) = entry {
                                if provider.kind == SourceControlProviderKind::Github {
                                    GitHubToken { state, saved, saving: *saving.read(), writable: settable(), onsave: save }
                                    if provider.status == t3_contracts::SourceControlDiscoveryStatus::Available { GitHubAccounts { state, auth: provider.auth.clone(), saving: *saving.read(), writable: settable(), onsave: save } }
                                } else if provider.kind == SourceControlProviderKind::Bitbucket {
                                    BitbucketCredentials { state, saved, saving: *saving.read(), writable: settable(), onsave: save }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn DiscoveryRow(entry: Entry, revealed: bool, details: bool, onreveal: EventHandler<()>, children: Element) -> Element {
    let mut expanded = use_signal(|| false);
    let item = entry.item();
    let label = item.label().to_owned();
    let dot = match item.dot() {
        Dot::Off => "sc-dot off",
        Dot::Warning => "sc-dot warning",
        Dot::Ok => "sc-dot ok",
    };
    let has_details = details;
    rsx! { article { class: "sc-row", "data-kind": "{item.key()}",
        div { class: "sc-main",
            div { class: "sc-title",
                span { class: dot, "aria-hidden": "true" }
                strong { "{label}" }
                if let Some(version) = item.version() { code { class: "muted", "{version}" } }
                if let Some(badge) = item.badge() { span { class: if item.not_ready() || item.badge() == Some("Not authenticated") { "badge warning" } else { "badge" }, "{badge}" } }
            }
            p { class: "sc-summary muted",
                for (index, part) in item.summary().into_iter().enumerate() {
                    match part {
                        Part::Text(text) => rsx! { span { key: "{index}", "{text}" } },
                        Part::Code(code) => rsx! { code { key: "{index}", "{code}" } },
                        Part::Account(account) => rsx! { button { key: "{index}", class: "account-reveal", r#type: "button", "aria-label": "Toggle source control account visibility", title: if revealed { "Click to hide account" } else { "Click to reveal account" }, onclick: move |_| onreveal.call(()),
                            if revealed { "{account}" } else { "{logic::redacted_placeholder(&account)}" } } },
                    }
                }
            }
        }
        div { class: "sc-side",
            if has_details { button { r#type: "button", "aria-label": "Toggle {label} details", "aria-expanded": "{expanded()}", onclick: move |_| expanded.toggle(), if expanded() { "Hide" } else { "Details" } } }
            if !item.not_ready() { input { r#type: "checkbox", disabled: true, checked: item.enabled(), "aria-label": "{label} availability" } }
        }
        if has_details && expanded() { div { class: "sc-details", {children} } }
    } }
}

fn send_patch(onsave: EventHandler<Value>, patch: Value) {
    onsave.call(patch);
}

#[component]
fn GitHubToken(state: Store<UiModel>, saved: Signal<u64>, saving: bool, writable: bool, onsave: EventHandler<Value>) -> Element {
    let tokens = use_memo(move || state.config().read()["settings"]["github"]["tokens"].clone());
    let mut host = use_signal(|| logic::DEFAULT_GITHUB_HOST.to_owned());
    let mut draft = use_signal(String::new);
    use_effect(move || {
        if saved() > 0 {
            draft.set(String::new());
        }
    });
    let normalized = logic::normalize_host(&host.read());
    let saved_hosts = logic::saved_token_hosts(&tokens.read());
    let is_saved = saved_hosts.contains(&normalized);
    let shown_host = if normalized.is_empty() { logic::DEFAULT_GITHUB_HOST.to_owned() } else { normalized.clone() };
    let submit = {
        let normalized = normalized.clone();
        move || {
            let token = draft.peek().trim().to_owned();
            if !normalized.is_empty() && !token.is_empty() && writable {
                send_patch(onsave, logic::github_token_patch(&normalized, &token));
            }
        }
    };
    rsx! { form { class: "sc-form", "aria-label": "GitHub token settings",
        onsubmit: { let submit = submit.clone(); move |event| { event.prevent_default(); submit() } },
        p { class: "muted", "A token saved here is used before " code { "GH_TOKEN" } " and the " code { "gh" } " login, so GitHub works without the GitHub CLI. Give it read and write access to pull requests and contents. "
            a { href: logic::new_github_token_url(&shown_host), target: "_blank", rel: "noreferrer noopener", "Create a token" } }
        div { class: "field-row",
            div { label { r#for: "github-token-host", "Host" } input { id: "github-token-host", "aria-label": "GitHub token host", autocomplete: "off", placeholder: logic::DEFAULT_GITHUB_HOST, value: "{host}", disabled: saving || !writable, oninput: move |event| host.set(event.value()) } }
            div { label { r#for: "github-token", "Token" } input { id: "github-token", r#type: "password", "aria-label": "GitHub token", autocomplete: "off", placeholder: if is_saved { "Stored secret, enter a new value to replace" } else { "Not set" }, value: "{draft}", disabled: saving || !writable, oninput: move |event| draft.set(event.value()) } }
        }
        div { class: "sc-form-footer",
            p { class: "muted", if saved_hosts.is_empty() { "No token saved; the server uses GH_TOKEN or the gh login." } else { "Saved for {saved_hosts.join(\", \")}." } }
            if is_saved { button { r#type: "button", "aria-label": "Remove GitHub token", disabled: saving || !writable, onclick: { let normalized = normalized.clone(); move |_| send_patch(onsave, logic::github_token_patch(&normalized, "")) }, "Remove" } }
            button { class: "primary", r#type: "button", "aria-label": "Save GitHub token", disabled: normalized.is_empty() || draft.read().trim().is_empty() || saving || !writable, onclick: { let submit = submit.clone(); move |_| submit() }, "Save" }
        }
    } }
}

#[component]
fn GitHubAccounts(state: Store<UiModel>, auth: t3_contracts::SourceControlProviderAuth, saving: bool, writable: bool, onsave: EventHandler<Value>) -> Element {
    let hosts = use_memo(move || state.config().read()["settings"]["github"]["hosts"].clone());
    let mut revealed = use_signal(|| false);
    let groups = logic::group_github_accounts(auth.accounts.as_deref().unwrap_or_default());
    if groups.is_empty() {
        return rsx! { p { class: "muted", "Sign in with " code { "gh auth login" } " on the server host, then rescan to choose accounts here." } };
    }
    let show = |account: &str| if *revealed.read() { account.to_owned() } else { logic::redacted_placeholder(account) };
    let change = move |host: String, change: HostChange| send_patch(onsave, logic::github_hosts_patch(logic::next_github_hosts(&hosts.peek(), &host, &change)));
    rsx! { div { class: "sc-accounts",
        div { class: "sc-form-footer",
            p { class: "muted", "Choose which " code { "gh" } " login each GitHub host uses, or turn a host off." }
            button { r#type: "button", "aria-label": if revealed() { "Hide GitHub accounts" } else { "Reveal GitHub accounts" }, "aria-pressed": "{revealed()}", onclick: move |_| revealed.toggle(), if revealed() { "Hide" } else { "Reveal" } }
        }
        for group in groups {
            div { key: "{group.host}", class: "sc-host", "data-host": "{group.host}",
                {
                    let choice = hosts.read()[&group.host].clone();
                    let enabled = choice["enabled"].as_bool().unwrap_or(true);
                    let stale = logic::stale_pin(&choice, &group);
                    let pinned = choice["account"].as_str().filter(|_| !stale).unwrap_or(ACTIVE).to_owned();
                    let active_label = match group.active_account.as_ref() {
                        Some(account) => format!("Active gh account ({})", show(account)),
                        None => "Active gh account".to_owned(),
                    };
                    rsx! {
                        div { class: "sc-form-footer",
                            div { strong { "{group.host}" } if group.selectable.len() == 1 { p { class: "muted", "Signed in as " span { "{show(&group.selectable[0])}" } } } }
                            label { class: "check", input { r#type: "checkbox", role: "switch", "aria-label": "Use GitHub on {group.host}", checked: enabled, disabled: saving || !writable, onchange: { let host = group.host.clone(); move |event| change(host.clone(), HostChange { enabled: Some(event.checked()), account: None }) } } if enabled { "On" } else { "Off" } }
                        }
                        if group.selectable.len() > 1 {
                            div { class: "sc-form-footer",
                                span { class: "muted", "Account" }
                                select { "aria-label": "GitHub account for {group.host}", value: "{pinned}", disabled: saving || !writable || !enabled,
                                    onchange: { let host = group.host.clone(); move |event| { let value = event.value(); change(host.clone(), HostChange { enabled: None, account: Some((value != ACTIVE).then_some(value)) }) } },
                                    option { value: ACTIVE, selected: pinned == ACTIVE, "{active_label}" }
                                    for account in group.selectable.iter() { option { key: "{account}", value: "{account}", selected: *account == pinned, "{show(account)}" } }
                                }
                            }
                        }
                        if stale {
                            div { class: "sc-form-footer",
                                p { class: "warning-text", "The chosen login is no longer signed in, so the active gh login is used." }
                                button { r#type: "button", "aria-label": "Use active login for {group.host}", disabled: saving || !writable, onclick: { let host = group.host.clone(); move |_| change(host.clone(), HostChange { enabled: None, account: Some(None) }) }, "Use active login" }
                            }
                        }
                        for entry in group.broken.iter() {
                            p { key: "{entry.account}", class: "muted", "{show(&entry.account)} can\u{2019}t be used: {entry.error.clone().unwrap_or_else(|| \"gh reports this login as invalid.\".into())}" }
                        }
                        if let Some(variable) = group.environment_variable.clone() { p { class: "warning-text", "{variable} is set on the server, so it overrides the account chosen here until it is unset." } }
                    }
                }
            }
        }
    } }
}
/// The select value for "follow gh's active login"; logins never contain spaces.
const ACTIVE: &str = "active gh account";

#[component]
fn BitbucketCredentials(state: Store<UiModel>, saved: Signal<u64>, saving: bool, writable: bool, onsave: EventHandler<Value>) -> Element {
    let stored = use_memo(move || state.config().read()["settings"]["bitbucket"].clone());
    let mut draft = use_signal(BitbucketDraft::default);
    use_effect(move || {
        if saved() > 0 {
            draft.set(BitbucketDraft::default());
        }
    });
    let saved = stored.read().clone();
    let current = draft.read().clone();
    let method = current.method(&saved);
    let saved_credential = logic::saved_credential(&saved);
    let can_save = current.can_save(&saved) && !saving && writable;
    let (link, link_label) = method.link();
    let patch = current.patch(&saved);
    let submit = move || {
        if can_save {
            if let Some(patch) = patch.clone() {
                send_patch(onsave, logic::bitbucket_patch(patch));
            }
        }
    };
    rsx! { form { class: "sc-form", "aria-label": "Bitbucket credentials form",
        onsubmit: { let submit = submit.clone(); move |event| { event.prevent_default(); submit() } },
        div { class: "schedule-mode", role: "group", "aria-label": "Bitbucket sign-in method",
            for credential in [Credential::AccessToken, Credential::ApiToken] {
                button { key: "{credential.label()}", r#type: "button", class: if method == credential { "selected" } else { "" }, "aria-pressed": "{method == credential}", "aria-label": "Use {credential.label().to_lowercase()}", disabled: saving || !writable, onclick: move |_| draft.write().choice = Some(credential), "{credential.label()}" }
            }
        }
        p { class: "muted", "{method.description()} " a { href: link, target: "_blank", rel: "noreferrer noopener", "{link_label}" } }
        if method == Credential::AccessToken {
            label { r#for: "bitbucket-access-token", "Access token" }
            input { id: "bitbucket-access-token", r#type: "password", "aria-label": "Bitbucket access token", autocomplete: "off", placeholder: if saved_credential == Some(Credential::AccessToken) { "Stored secret, enter a new value to replace" } else { "Not set" }, value: "{current.access_token}", disabled: saving || !writable, oninput: move |event| draft.write().access_token = event.value() }
        } else {
            label { r#for: "bitbucket-email", "Atlassian account email" }
            input { id: "bitbucket-email", r#type: "email", "aria-label": "Atlassian account email", autocomplete: "off", placeholder: "you@example.com", value: "{current.email(&saved)}", disabled: saving || !writable, oninput: move |event| draft.write().email = Some(event.value()) }
            label { r#for: "bitbucket-api-token", "API token" }
            input { id: "bitbucket-api-token", r#type: "password", "aria-label": "Bitbucket API token", autocomplete: "off", placeholder: if saved_credential == Some(Credential::ApiToken) { "Stored secret, enter a new value to replace" } else { "Not set" }, value: "{current.api_token}", disabled: saving || !writable, oninput: move |event| draft.write().api_token = event.value() }
        }
        div { class: "sc-form-footer",
            p { class: "muted", if let Some(footer) = logic::bitbucket_footer(&saved, &current) { "{footer}" } }
            if saved_credential.is_some() { button { r#type: "button", "aria-label": "Remove Bitbucket credentials", disabled: saving || !writable, onclick: move |_| send_patch(onsave, logic::bitbucket_patch(json!({"accessToken":"","email":"","apiToken":""}))), "Remove" } }
            button { class: "primary", r#type: "button", "aria-label": "Save Bitbucket credentials", disabled: !can_save, onclick: { let submit = submit.clone(); move |_| submit() }, "Save" }
        }
    } }
}

/// How often the server fetches remote branches in the background. It lives in the background
/// activity policy, which still decides whether a fetch may run when the interval comes round.
#[component]
fn FetchInterval(state: Store<UiModel>, saving: bool, writable: bool, onsave: EventHandler<Value>) -> Element {
    use t3_client::text_settings as interval;
    let activity = use_memo(move || state.config().read()["settings"]["backgroundActivity"].clone());
    let known = state.config().read()["settings"]["backgroundActivity"].is_object();
    let seconds = interval::fetch_seconds(&activity.read());
    let changed = seconds != interval::preset_seconds(&activity.read());
    let off = saving || !writable || !known;
    rsx! {
        section { class: "sc-section", "aria-label": "Git fetch interval",
            h2 { "Git fetch interval" }
            p { class: "muted", "Refresh remote branches in the background. Set to 0 to avoid automatic Git prompts. This interval is configured for Git only; the shared Background activity policy still decides whether Git refreshes may run when the timer fires." }
            div { class: "field-row",
                button { r#type: "button", "aria-label": "Decrease fetch interval", disabled: off || seconds == 0, onclick: move |_| onsave.call(interval::fetch_patch(&activity.peek(), Some(seconds.saturating_sub(interval::FETCH_STEP_SECONDS)))), "\u{2212}" }
                input { r#type: "number", min: "0", step: "{interval::FETCH_STEP_SECONDS}", "aria-label": "Automatic Git fetch interval in seconds", value: "{seconds}", disabled: off,
                    onchange: move |event| { if let Some(next) = interval::parse_seconds(&event.value()).filter(|next| *next != seconds) { onsave.call(interval::fetch_patch(&activity.peek(), Some(next))) } } }
                button { r#type: "button", "aria-label": "Increase fetch interval", disabled: off, onclick: move |_| onsave.call(interval::fetch_patch(&activity.peek(), Some(seconds + interval::FETCH_STEP_SECONDS))), "+" }
                span { class: "muted", "seconds" }
                if changed { button { r#type: "button", "aria-label": "Reset fetch interval", disabled: off, onclick: move |_| onsave.call(interval::fetch_patch(&activity.peek(), None)), "Reset" } }
            }
        }
    }
}
