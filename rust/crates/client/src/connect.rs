//! T3 Connect on the Connections page: what the environment says about its link, what a switch asks for, and the relay
//! client's installation. Port of the logic of `cloud/useCloudLinkController.ts` (`reconcileCloudState`),
//! `cloud/linkEnvironment.ts` (`readPrimaryCloudLinkState`, `updatePrimaryCloudPreferences`,
//! `unlinkPrimaryEnvironmentFromCloud`, `ensureRelayClientAvailable`), `cloud/relayClientInstallDialog.ts` and the steps of
//! `components/cloud/RelayClientInstallDialog.tsx`, and of the rows of `ConnectionsSettings.tsx` (`ConfiguredCloudLinkRow`).
//!
//! The original signs in to the hosted service in the browser (Clerk) and sends that sign-in's token to the relay when it
//! links an environment. This app has no such sign-in, so what needs the token (linking, changing between a managed tunnel and
//! publishing alone) is `Plan::SignIn`, and everything that only talks to this server (turning the link off, the preferences of
//! a link that exists) is planned and done.
use serde::Deserialize;
use serde_json::{Value, json};

pub const LINK_STATE: &str = "/api/connect/link-state";
pub const PREFERENCES: &str = "/api/connect/preferences";
pub const UNLINK: &str = "/api/connect/unlink";
pub const RELAY_CLIENT_STATUS: &str = "cloud.getRelayClientStatus";
pub const INSTALL_RELAY_CLIENT: &str = "cloud.installRelayClient";

/// `EnvironmentCloudLinkStateResult`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkState {
    pub linked: bool,
    #[serde(default)]
    pub cloud_user_id: Option<String>,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub relay_issuer: Option<String>,
    #[serde(default)]
    pub managed_tunnel_active: Option<bool>,
    #[serde(default)]
    pub publish_agent_activity: bool,
    #[serde(default)]
    pub hold_webhooks_while_offline: Option<bool>,
}
impl LinkState {
    pub fn parse(value: &Value) -> Result<Self, String> {
        serde_json::from_value(value.clone()).map_err(|error| format!("Could not read T3 Connect link state: {error}"))
    }
    /// Older servers predate the field; for them a link always implies a managed tunnel.
    pub fn managed_tunnel(&self) -> bool {
        self.managed_tunnel_active.unwrap_or(self.linked)
    }
    pub fn hold_webhooks(&self) -> bool {
        self.hold_webhooks_while_offline.unwrap_or(false)
    }
}

/// The full state a switch asks for (`CloudLinkDesiredState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desired {
    pub managed_tunnel: bool,
    pub publish: bool,
    /// None leaves the setting as it is.
    pub hold_webhooks: Option<bool>,
}

/// What it takes to reach a desired state from the current one.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Neither the tunnel nor publishing is wanted: unlink. Works without the relay's sign-in.
    Unlink,
    /// Set the preferences (the body of `POST /api/connect/preferences`).
    Preferences(Value),
    /// The link has to be made, or changed between managed and publish-only, with the sign-in's token.
    SignIn,
}

pub const SIGN_IN_REQUIRED: &str = "Sign in to T3 Connect before enabling this.";

/// `reconcileCloudState`: unlink when neither the tunnel nor publishing is wanted; otherwise link again when the
/// environment is not linked or the tunnel mode differs, and then set the preferences.
pub fn plan(current: &LinkState, desired: Desired) -> Plan {
    if !(desired.managed_tunnel || desired.publish) {
        return Plan::Unlink;
    }
    if !current.linked || current.managed_tunnel() != desired.managed_tunnel {
        return Plan::SignIn;
    }
    let mut body = json!({"publishAgentActivity": desired.publish});
    if let Some(hold) = desired.hold_webhooks {
        body["holdWebhooksWhileOffline"] = hold.into();
    }
    Plan::Preferences(body)
}

/// Which switch of the section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    ManagedTunnel,
    PublishAgentActivity,
    HoldWebhooks,
}
impl Switch {
    pub fn desired(self, current: &LinkState, enabled: bool) -> Desired {
        let base = Desired { managed_tunnel: current.managed_tunnel(), publish: current.publish_agent_activity, hold_webhooks: None };
        match self {
            Self::ManagedTunnel => Desired { managed_tunnel: enabled, ..base },
            Self::PublishAgentActivity => Desired { publish: enabled, ..base },
            Self::HoldWebhooks => Desired { hold_webhooks: Some(enabled), ..base },
        }
    }
    /// The title of the switch, which names its control.
    pub fn label(self) -> &'static str {
        match self {
            Self::ManagedTunnel => "Enable T3 Connect",
            Self::PublishAgentActivity => "Publish agent activity to mobile clients",
            Self::HoldWebhooks => "Hold webhook requests while this environment is offline",
        }
    }
}

pub const SIGN_IN_REASON: &str = "This app cannot sign in to T3 Connect. Link this environment with `t3 connect`, or from the T3 Code app.";
pub const PERMISSION_REASON: &str = "Your session does not have permission to manage T3 Connect access.";
pub const NO_READ_NOTICE: &str = "To edit these settings, pair this connection with both View relay and Manage relay permissions.";

/// Why a switch cannot be used, when it cannot (`disabledReason` of the original's `CloudLinkSwitch`, where this app lacks the
/// sign-in only for the changes that need the relay's token).
pub fn disabled_reason(current: Option<&LinkState>, can_manage: bool, which: Switch, enabled: bool) -> Option<&'static str> {
    let current = current?;
    if !can_manage {
        return Some(PERMISSION_REASON);
    }
    (plan(current, which.desired(current, !enabled)) == Plan::SignIn).then_some(SIGN_IN_REASON)
}

/// The toast after a change succeeded (`updateManagedTunnel`, `updatePublishAgentActivity`, `updateHoldWebhooks`).
pub fn success_message(which: Switch, enabled: bool, publishing: bool) -> (&'static str, &'static str) {
    match (which, enabled) {
        (Switch::ManagedTunnel, true) => ("T3 Connect linked", "This environment is available through T3 Connect."),
        (Switch::ManagedTunnel, false) if publishing => ("T3 Connect tunnel disabled", "The managed tunnel was removed. Agent activity publishing stays on."),
        (Switch::ManagedTunnel, false) => ("T3 Connect unlinked", "This environment is no longer available through T3 Connect."),
        (Switch::PublishAgentActivity, true) => ("Agent activity enabled", "This environment publishes agent activity to your mobile clients."),
        (Switch::PublishAgentActivity, false) => ("Agent activity disabled", "This environment will stop publishing agent activity."),
        (Switch::HoldWebhooks, true) => ("Webhooks held while offline", "T3 Connect keeps webhook requests for up to 24 hours while this environment is offline."),
        (Switch::HoldWebhooks, false) => ("Webhooks no longer held", "Requests to an offline environment now fail. Anything already held is still delivered."),
    }
}

/// `environmentApiError`: the failure the way the original words it, with the server's own message after the action.
pub fn failure_message(action: &str, server_message: &str) -> String {
    format!("{}: {server_message}", action.trim_end_matches(['.', ':']))
}
pub const PREFERENCES_FAILURE: &str = "Could not update environment cloud preferences.";
pub const UNLINK_FAILURE: &str = "Could not unlink the environment from cloud.";
pub const READ_FAILURE: &str = "Could not read environment cloud link state.";

/// `RelayClientStatusSchema`.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayClient {
    Available,
    Missing { version: String },
    Unsupported { platform: String, arch: String },
}
impl RelayClient {
    pub fn parse(value: &Value) -> Result<Self, String> {
        let text = |name: &str| value[name].as_str().map(str::to_owned);
        match value["status"].as_str() {
            Some("available") => Ok(Self::Available),
            Some("missing") => text("version").map(|version| Self::Missing { version }).ok_or_else(|| "The relay client status has no version.".into()),
            Some("unsupported") => match (text("platform"), text("arch")) {
                (Some(platform), Some(arch)) => Ok(Self::Unsupported { platform, arch }),
                _ => Err("The relay client status has no platform.".into()),
            },
            _ => Err("The server answered the relay client status with something this app does not read.".into()),
        }
    }
    /// `ensureRelayClientAvailable`'s words for a platform it cannot install on.
    pub fn unsupported_message(platform: &str, arch: &str) -> String {
        format!("T3 Code cannot install the relay client automatically on {platform}-{arch}.")
    }
}

/// The steps of the installation, in the order the dialog lists them.
pub const INSTALL_STEPS: [(&str, &str); 7] = [
    ("checking", "Checking current installation"),
    ("waiting_for_lock", "Waiting for installer"),
    ("downloading", "Downloading relay client"),
    ("verifying", "Verifying download"),
    ("installing", "Installing relay client"),
    ("validating", "Validating executable"),
    ("activating", "Activating installation"),
];

/// Where a stage stands in the dialog: its label and its number of seven, when it is one of the seven.
pub fn install_step(stage: &str) -> Option<(&'static str, usize)> {
    INSTALL_STEPS.iter().position(|(name, _)| *name == stage).map(|index| (INSTALL_STEPS[index].1, index + 1))
}

/// What one event of `cloud.installRelayClient` says.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallEvent {
    Progress(String),
    Complete(RelayClient),
}
pub fn install_event(value: &Value) -> Result<InstallEvent, String> {
    match value["type"].as_str() {
        Some("progress") => value["stage"].as_str().map(|stage| InstallEvent::Progress(stage.to_owned())).ok_or_else(|| "A progress event of the relay client installation names no stage.".into()),
        Some("complete") => RelayClient::parse(&value["status"]).map(InstallEvent::Complete),
        _ => Err("The relay client installation sent an event this app does not read.".into()),
    }
}

/// How the installation ended, in the original's words (`ensureRelayClientAvailable`).
pub fn install_outcome(last: Option<&InstallEvent>) -> Result<(), String> {
    match last {
        Some(InstallEvent::Complete(RelayClient::Available)) => Ok(()),
        Some(InstallEvent::Complete(RelayClient::Unsupported { platform, arch })) => Err(RelayClient::unsupported_message(platform, arch)),
        Some(InstallEvent::Complete(RelayClient::Missing { .. })) => Err("The relay client is still unavailable after installation.".into()),
        _ => Err("The relay client install completed without a final status.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(value: Value) -> LinkState {
        LinkState::parse(&value).unwrap()
    }
    fn unlinked() -> LinkState {
        state(json!({"linked":false,"cloudUserId":null,"relayUrl":null,"relayIssuer":null,"managedTunnelActive":false,"publishAgentActivity":false,"holdWebhooksWhileOffline":false}))
    }
    fn managed() -> LinkState {
        state(json!({"linked":true,"cloudUserId":"user-1","relayUrl":"https://relay.example","relayIssuer":"https://relay.example","managedTunnelActive":true,"publishAgentActivity":false,"holdWebhooksWhileOffline":false}))
    }

    #[test]
    fn turning_both_capabilities_off_unlinks_and_anything_else_needs_a_link_in_the_right_mode() {
        assert_eq!(plan(&managed(), Desired { managed_tunnel: false, publish: false, hold_webhooks: None }), Plan::Unlink);
        assert_eq!(plan(&unlinked(), Desired { managed_tunnel: false, publish: false, hold_webhooks: None }), Plan::Unlink);
        // Not linked: publishing alone, or the tunnel, is a link to make.
        assert_eq!(plan(&unlinked(), Desired { managed_tunnel: false, publish: true, hold_webhooks: None }), Plan::SignIn);
        assert_eq!(plan(&unlinked(), Desired { managed_tunnel: true, publish: false, hold_webhooks: None }), Plan::SignIn);
        // Linked with a tunnel: publishing is a preference; dropping the tunnel while publishing is a new, publish-only link.
        assert_eq!(plan(&managed(), Desired { managed_tunnel: true, publish: true, hold_webhooks: None }), Plan::Preferences(json!({"publishAgentActivity":true})));
        assert_eq!(plan(&managed(), Desired { managed_tunnel: false, publish: true, hold_webhooks: None }), Plan::SignIn);
        // The webhook choice rides along only when asked for.
        assert_eq!(plan(&managed(), Desired { managed_tunnel: true, publish: false, hold_webhooks: Some(true) }), Plan::Preferences(json!({"publishAgentActivity":false,"holdWebhooksWhileOffline":true})));
    }

    #[test]
    fn an_older_server_without_the_tunnel_field_is_linked_with_a_tunnel() {
        let older = state(json!({"linked":true,"publishAgentActivity":false}));
        assert!(older.managed_tunnel());
        assert!(!older.hold_webhooks());
        assert!(!state(json!({"linked":false,"publishAgentActivity":false})).managed_tunnel());
        assert!(LinkState::parse(&json!({"cloudUserId":null})).is_err());
    }

    #[test]
    fn a_switch_is_unavailable_without_permission_and_where_the_change_needs_the_sign_in() {
        let managed = managed();
        let unlinked = unlinked();
        assert_eq!(disabled_reason(None, true, Switch::PublishAgentActivity, false), None, "nothing is known yet: the page shows it loading");
        assert_eq!(disabled_reason(Some(&managed), false, Switch::PublishAgentActivity, false), Some(PERMISSION_REASON));
        assert_eq!(disabled_reason(Some(&managed), true, Switch::PublishAgentActivity, false), None);
        assert_eq!(disabled_reason(Some(&managed), true, Switch::HoldWebhooks, false), None);
        // Turning the tunnel off while publishing would be a publish-only link.
        let publishing = LinkState { publish_agent_activity: true, ..managed.clone() };
        assert_eq!(disabled_reason(Some(&publishing), true, Switch::ManagedTunnel, true), Some(SIGN_IN_REASON));
        assert_eq!(disabled_reason(Some(&managed), true, Switch::ManagedTunnel, true), None, "turning the only capability off is an unlink");
        assert_eq!(disabled_reason(Some(&unlinked), true, Switch::ManagedTunnel, false), Some(SIGN_IN_REASON));
        assert_eq!(disabled_reason(Some(&unlinked), true, Switch::PublishAgentActivity, false), Some(SIGN_IN_REASON));
    }

    #[test]
    fn the_toast_after_a_change_says_what_stays_on() {
        assert_eq!(success_message(Switch::ManagedTunnel, false, true).0, "T3 Connect tunnel disabled");
        assert_eq!(success_message(Switch::ManagedTunnel, false, false).0, "T3 Connect unlinked");
        assert_eq!(success_message(Switch::HoldWebhooks, true, false).0, "Webhooks held while offline");
    }

    #[test]
    fn a_failure_is_worded_with_the_servers_message_after_the_action() {
        assert_eq!(failure_message(PREFERENCES_FAILURE, "Link this environment to T3 Connect first."), "Could not update environment cloud preferences: Link this environment to T3 Connect first.");
    }

    #[test]
    fn the_relay_client_status_and_the_installation_are_read_as_the_original_reads_them() {
        assert_eq!(RelayClient::parse(&json!({"status":"missing","version":"2025.1.0"})), Ok(RelayClient::Missing { version: "2025.1.0".into() }));
        assert_eq!(RelayClient::parse(&json!({"status":"available","executablePath":"/x","source":"managed","version":"1"})), Ok(RelayClient::Available));
        assert!(RelayClient::parse(&json!({"status":"missing"})).is_err());
        assert_eq!(install_step("verifying"), Some(("Verifying download", 4)));
        assert_eq!(install_step("nope"), None);
        let progress = install_event(&json!({"type":"progress","stage":"downloading"})).unwrap();
        assert_eq!(progress, InstallEvent::Progress("downloading".into()));
        let done = install_event(&json!({"type":"complete","status":{"status":"available","executablePath":"/x","source":"managed","version":"1"}})).unwrap();
        assert_eq!(install_outcome(Some(&done)), Ok(()));
        assert_eq!(install_outcome(Some(&progress)), Err("The relay client install completed without a final status.".into()));
        assert_eq!(install_outcome(None), Err("The relay client install completed without a final status.".into()));
        let unsupported = InstallEvent::Complete(RelayClient::Unsupported { platform: "plan9".into(), arch: "mips".into() });
        assert_eq!(install_outcome(Some(&unsupported)), Err("T3 Code cannot install the relay client automatically on plan9-mips.".into()));
        assert_eq!(install_outcome(Some(&InstallEvent::Complete(RelayClient::Missing { version: "1".into() }))), Err("The relay client is still unavailable after installation.".into()));
    }
}
