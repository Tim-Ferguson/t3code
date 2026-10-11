//! What `t3 connect link` wants, kept in the secret store so the server can act on it at startup
//! (`cloud/CliState.ts`).

use crate::secrets::*;

/// "managed" provisions a Cloudflare tunnel (the default; legacy value `true`). "publish_only"
/// links the environment purely to publish agent activity: no tunnel and no relay-advertised
/// endpoint, so activity reaches mobile clients that connect out of band (Tailscale, pairing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesiredLinkMode {
    #[default]
    Managed,
    PublishOnly,
}

pub fn read_cli_desired_cloud_link(store: &dyn SecretStore) -> Result<bool, SecretError> {
    Ok(store.get(CLOUD_CLI_DESIRED_LINK_SECRET)?.is_some())
}

pub fn read_cli_desired_link_mode(store: &dyn SecretStore) -> Result<DesiredLinkMode, SecretError> {
    Ok(match store.get(CLOUD_CLI_DESIRED_LINK_SECRET)? {
        Some(value) if value == b"publish_only" => DesiredLinkMode::PublishOnly,
        // Legacy links stored the literal "true" and are always managed.
        _ => DesiredLinkMode::Managed,
    })
}

pub fn set_cli_desired_cloud_link(
    store: &dyn SecretStore,
    desired: bool,
    mode: DesiredLinkMode,
) -> Result<(), SecretError> {
    if desired {
        let value: &[u8] = match mode {
            DesiredLinkMode::PublishOnly => b"publish_only",
            DesiredLinkMode::Managed => b"managed",
        };
        store.set(CLOUD_CLI_DESIRED_LINK_SECRET, value)
    } else {
        store.remove(CLOUD_CLI_DESIRED_LINK_SECRET)
    }
}

/// Forgets the link and the CLI's wish for it. Does not touch the CLI's OAuth token.
/// TODO(original bug, cloud/CliState.ts clearPersistedCloudLink): unlike `CloudLink.unlink`, this
/// leaves `cloud-hold-webhooks-while-offline` behind, so a relink after `t3 connect unlink`
/// inherits the old opt-in locally while the relay has dropped it.
pub fn clear_persisted_cloud_link(store: &dyn SecretStore) -> Result<(), SecretError> {
    for name in [
        CLOUD_CLI_DESIRED_LINK_SECRET,
        CLOUD_LINKED_USER_ID,
        RELAY_URL_SECRET,
        RELAY_ISSUER_SECRET,
        RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
        CLOUD_MINT_PUBLIC_KEY,
        CLOUD_ENDPOINT_RUNTIME_CONFIG,
        CLOUD_ENDPOINT_CONFIRMED_ORIGIN,
        PUBLISH_AGENT_ACTIVITY_SECRET,
    ] {
        store.remove(name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_desired_exposure_and_clears_provisioned_relay_state() {
        let store = MemorySecretStore::new();
        assert!(!read_cli_desired_cloud_link(&store).unwrap());
        set_cli_desired_cloud_link(&store, true, DesiredLinkMode::Managed).unwrap();
        assert!(read_cli_desired_cloud_link(&store).unwrap());
        for name in [
            CLOUD_LINKED_USER_ID,
            RELAY_URL_SECRET,
            RELAY_ISSUER_SECRET,
            RELAY_ENVIRONMENT_CREDENTIAL_SECRET,
            CLOUD_MINT_PUBLIC_KEY,
            CLOUD_ENDPOINT_RUNTIME_CONFIG,
            CLOUD_ENDPOINT_CONFIRMED_ORIGIN,
            PUBLISH_AGENT_ACTIVITY_SECRET,
        ] {
            store.set(name, name.as_bytes()).unwrap();
        }
        clear_persisted_cloud_link(&store).unwrap();
        assert!(!read_cli_desired_cloud_link(&store).unwrap());
        assert!(store.names().is_empty());
    }

    #[test]
    fn round_trips_the_mode_and_defaults_legacy_links_to_managed() {
        let store = MemorySecretStore::new();
        assert_eq!(
            read_cli_desired_link_mode(&store).unwrap(),
            DesiredLinkMode::Managed
        );
        set_cli_desired_cloud_link(&store, true, DesiredLinkMode::PublishOnly).unwrap();
        assert!(read_cli_desired_cloud_link(&store).unwrap());
        assert_eq!(
            read_cli_desired_link_mode(&store).unwrap(),
            DesiredLinkMode::PublishOnly
        );
        set_cli_desired_cloud_link(&store, true, DesiredLinkMode::Managed).unwrap();
        assert_eq!(
            read_cli_desired_link_mode(&store).unwrap(),
            DesiredLinkMode::Managed
        );
        store.set(CLOUD_CLI_DESIRED_LINK_SECRET, b"true").unwrap();
        assert!(read_cli_desired_cloud_link(&store).unwrap());
        assert_eq!(
            read_cli_desired_link_mode(&store).unwrap(),
            DesiredLinkMode::Managed
        );
        set_cli_desired_cloud_link(&store, false, DesiredLinkMode::Managed).unwrap();
        assert_eq!(
            read_cli_desired_link_mode(&store).unwrap(),
            DesiredLinkMode::Managed
        );
    }
}
